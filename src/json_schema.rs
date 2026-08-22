// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `json-schema` module — native JSON extraction + JSON-Schema validation.
//!
//! Provides four functions exposed as `import { jsonSchema } from "json-schema"`:
//!   - `jsonSchema.extract(text)`       — three-stage JSON-from-text extraction
//!   - `jsonSchema.validate(v, s, m)`   — recursive JSON-Schema validation/coerce
//!   - `jsonSchema.schemaExample(s)`    — synthesize an example value from a schema
//!   - `jsonSchema.matchesType(v, t)`   — JSON-Schema type keyword matcher

use zuqe::{Array, CallContext, FieldDef, JSResult, JSValue, ScopedContext};

use crate::helpers::{
    json_schema_coerce_or_check, json_schema_example, json_schema_matches_type_field,
};

/// Convert a serde_json::Value to a JSValue.
fn to_js(ctx: &mut ScopedContext, v: &serde_json::Value) -> JSResult {
    JSValue::from_serde_json(ctx, v)
}

/// Convert a JSValue to a serde_json::Value.
fn from_js(ctx: &mut ScopedContext, v: &JSValue) -> Result<serde_json::Value, String> {
    v.to_serde_json(ctx)
        .map_err(|e| format!("cannot convert to JSON: {e}"))
}

/// Extract the first JSON value from free-form text (strict → fenced → bare).
fn extract_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Stage 1: strict — parse the entire trimmed text.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Some(v);
    }

    // Stage 2: fenced — ```json ... ``` or ``` ... ```.
    if let Some(v) = extract_fenced(trimmed) {
        return Some(v);
    }

    // Stage 3: bare { ... } — find balanced braces.
    if let Some(v) = extract_bare_object(trimmed) {
        return Some(v);
    }

    None
}

/// Find ``` (optionally followed by "json") then extract and parse the content
/// between the opening and closing ``` fences.
fn extract_fenced(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i + 3 < len {
        // Find opening ```
        if bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
            let mut j = i + 3;
            // Skip optional "json" language tag and whitespace after opening fence.
            while j < len && bytes[j] != b'\n' {
                j += 1;
            }
            if j < len {
                j += 1; // skip \n
            }
            let content_start = j;
            // Find closing ``` at start of a line.
            while j + 3 <= len {
                if bytes[j] == b'`' && bytes[j + 1] == b'`' && bytes[j + 2] == b'`' {
                    // Ensure this is at line start (start of string or preceded by \n).
                    if j == 0 || bytes[j - 1] == b'\n' {
                        let content = &text[content_start..j].trim();
                        if !content.is_empty() {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
                                return Some(v);
                            }
                        }
                        break;
                    }
                    j += 3;
                } else {
                    j += 1;
                }
            }
            break; // only try the first opening ```
        }
        i += 1;
    }
    None
}

/// Find the first `{` and match balanced braces to extract a JSON object.
fn extract_bare_object(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    for start in 0..len {
        if bytes[start] == b'{' {
            let mut depth = 0i32;
            let mut end = start;
            for i in start..len {
                match bytes[i] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth == 0 {
                let slice = &text[start..end];
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) {
                    return Some(v);
                }
            }
        }
    }
    None
}

// Schema type matching, recursive validation/coercion, and example synthesis
// live in `helpers/json_schema.rs` (shared with the MCP server).

// ── native function callbacks ──

/// `jsonSchema.extract(text: string): unknown`
fn js_json_schema_extractJSON(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text = cc.arg(0).to_string(ctx)?;
    let s = text.content().encode_utf8(false).to_string();

    match extract_json(&s) {
        Some(v) => to_js(ctx, &v),
        None => Ok(JSValue::UNDEFINED),
    }
}

/// `JSONSchema.validateSchema(value, schema, mode?): { ok, value, errors }`
fn js_json_schema_validateSchema(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let value_arg = cc.arg(0);
    let schema_arg = cc.arg(1);

    let value: serde_json::Value = from_js(ctx, &value_arg)
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.validate: {e}")))?;
    let schema: serde_json::Value = from_js(ctx, &schema_arg)
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.validate schema: {e}")))?;

    let mode: String = if cc.arg(2).is_undefined() {
        "best-effort".into()
    } else {
        cc.arg(2)
            .to_string(ctx)?
            .content()
            .encode_utf8(false)
            .to_string()
    };

    let coerce = mode == "coerce";
    let mut errors: Vec<String> = Vec::new();
    let out = json_schema_coerce_or_check(value, &schema, "root", &mut errors, coerce);

    // Build result object: { ok, value, errors }
    let mut result_obj = ctx.create_base_object()?;
    let ok = errors.is_empty();
    let js_value = to_js(ctx, &out)?;
    let js_errors = {
        let js_errs: Vec<JSValue> = errors
            .iter()
            .map(|e| {
                ctx.create_string_from_utf8(e.as_bytes().to_vec())
                    .map(JSValue::make_string)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Array::from_values(ctx, js_errs)?.into()
    };

    result_obj.define_property_value_str(ctx, "ok", JSValue::from(ok), zuqe::JS_PROP_ENUMERABLE)?;
    result_obj.define_property_value_str(ctx, "value", js_value, zuqe::JS_PROP_ENUMERABLE)?;
    result_obj.define_property_value_str(ctx, "errors", js_errors, zuqe::JS_PROP_ENUMERABLE)?;

    Ok(result_obj.into())
}

/// `JSONSchema.forExample(schema): unknown`
fn js_json_schema_forExample(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let schema: serde_json::Value = from_js(ctx, &cc.arg(0))
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.schemaExample: {e}")))?;

    let example = json_schema_example(&schema);
    to_js(ctx, &example)
}

/// `jsonSchema.matchesType(value, type): boolean`
fn js_json_schema_matchesType(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let value_arg = cc.arg(0);
    let type_arg = cc.arg(1);

    let value: serde_json::Value = from_js(ctx, &value_arg)
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.matchesType: {e}")))?;
    let type_val: serde_json::Value = from_js(ctx, &type_arg)
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.matchesType type arg: {e}")))?;

    let ok = json_schema_matches_type_field(&value, &type_val);
    Ok(JSValue::from(ok))
}

// ── module registration ──

const JSON_SCHEMA_FUNCS: &[FieldDef] = &[
    FieldDef::FUNC("extractJSON", 1, js_json_schema_extractJSON, 0),
    FieldDef::FUNC("validateSchema", 2, js_json_schema_validateSchema, 0),
    FieldDef::FUNC("forExample", 1, js_json_schema_forExample, 0),
    FieldDef::FUNC("matchesType", 2, js_json_schema_matchesType, 0),
];

const MOD_JSON_SCHEMA_ENTRIES: &[FieldDef] = &[FieldDef::OBJECT(
    "default",
    JSON_SCHEMA_FUNCS,
    zuqe::JS_PROP_ENUMERABLE,
)];

pub fn init_mod_json_schema(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("json-schema", MOD_JSON_SCHEMA_ENTRIES)?;
    Ok(())
}
