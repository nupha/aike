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

// ── helpers ──

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

// ── validate helpers ──

/// Check if a serde_json::Value matches a JSON-Schema type keyword.
fn matches_type(value: &serde_json::Value, schema_type: &str) -> bool {
    match schema_type {
        "string" => value.is_string(),
        "number" => value.is_f64() || value.is_i64() || value.is_u64(),
        "integer" => {
            value.is_i64() || value.is_u64() ||
            // serde_json doesn't distinguish integer from float, so check
            // that an f64 value is actually an integer.
            value.as_f64().map_or(false, |f| f.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true, // unknown keyword → accept
    }
}

/// Check if a value matches a type which may be a string or array of strings.
fn matches_type_field(value: &serde_json::Value, type_field: &serde_json::Value) -> bool {
    match type_field {
        serde_json::Value::String(s) => matches_type(value, s),
        serde_json::Value::Array(arr) => arr.iter().any(|t| {
            if let serde_json::Value::String(s) = t {
                matches_type(value, s)
            } else {
                false
            }
        }),
        _ => true, // no type specified → accept
    }
}

/// Recursively validate a value against a JSON-Schema, optionally coercing.
/// Returns the (possibly coerced) value and collects errors.
fn coerce_or_check(
    value: serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
    coerce: bool,
) -> serde_json::Value {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return value,
    };

    // Determine the schema type.
    let schema_type = obj.get("type");

    // Object type: schema declares type:"object", or has properties but no explicit type.
    let schema_is_object = schema_type.map_or(
        obj.contains_key("properties"), // no type field → infer object from properties
        |t| t.as_str() == Some("object"),
    );

    if schema_is_object {
        if !value.is_object() {
            errors.push(format!("{path}: expected object"));
            return value;
        }
        let props = obj.get("properties").and_then(|p| p.as_object());
        let required = obj.get("required").and_then(|r| r.as_array());
        if let Some(props) = props {
            let mut result = value.as_object().cloned().unwrap_or_default();
            for (k, prop_schema) in props {
                if let Some(v) = result.get(k) {
                    let coerced = coerce_or_check(
                        v.clone(),
                        prop_schema,
                        &format!("{path}.{k}"),
                        errors,
                        coerce,
                    );
                    result.insert(k.clone(), coerced);
                } else if let Some(req) = required {
                    if req.iter().any(|r| r.as_str() == Some(k.as_str())) {
                        errors.push(format!("{path}.{k}: missing required property"));
                    }
                }
            }
            return serde_json::Value::Object(result);
        }
        return value;
    }

    // Array type
    let schema_is_array = schema_type.map_or(false, |t| t.as_str() == Some("array"));
    if schema_is_array || (schema_type.is_none() && !value.is_null() && value.is_array()) {
        if !value.is_array() {
            errors.push(format!("{path}: expected array"));
            return value;
        }
        if let Some(items_schema) = obj.get("items") {
            let arr = value.as_array().cloned().unwrap_or_default();
            let coerced: Vec<serde_json::Value> = arr
                .into_iter()
                .enumerate()
                .map(|(i, v)| {
                    coerce_or_check(v, items_schema, &format!("{path}[{i}]"), errors, coerce)
                })
                .collect();
            return serde_json::Value::Array(coerced);
        }
        return value;
    }

    // Primitive types
    if let Some(type_field) = schema_type {
        if let Some(type_str) = type_field.as_str() {
            match type_str {
                "string" => {
                    if !matches_type(&value, "string") {
                        if coerce && (value.is_number() || value.is_boolean()) {
                            let s = match &value {
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::Bool(b) => b.to_string(),
                                _ => String::new(),
                            };
                            return serde_json::Value::String(s);
                        }
                        errors.push(format!("{path}: expected string"));
                    }
                    return value;
                }
                "number" | "integer" => {
                    if !matches_type(&value, type_str) {
                        if coerce {
                            let n = match &value {
                                serde_json::Value::String(s) => s.parse::<f64>().ok(),
                                serde_json::Value::Bool(true) => Some(1.0),
                                serde_json::Value::Bool(false) => Some(0.0),
                                _ => None,
                            };
                            if let Some(n) = n {
                                if type_str == "integer" && n.fract() != 0.0 {
                                    errors.push(format!("{path}: expected integer"));
                                }
                                if let Some(num) = serde_json::Number::from_f64(n) {
                                    return serde_json::Value::Number(num);
                                }
                            }
                        }
                        errors.push(format!("{path}: expected {type_str}"));
                        return value;
                    }
                    if type_str == "integer" && !matches_type(&value, "integer") {
                        errors.push(format!("{path}: expected integer"));
                    }
                    return value;
                }
                "boolean" => {
                    if !matches_type(&value, "boolean") {
                        if coerce {
                            match &value {
                                serde_json::Value::String(s) if s == "true" => {
                                    return serde_json::Value::Bool(true);
                                }
                                serde_json::Value::String(s) if s == "false" => {
                                    return serde_json::Value::Bool(false);
                                }
                                _ => {}
                            }
                        }
                        errors.push(format!("{path}: expected boolean"));
                    }
                    return value;
                }
                "null" => {
                    if !value.is_null() {
                        errors.push(format!("{path}: expected null"));
                    }
                    return value;
                }
                _ => return value, // unknown type → accept
            }
        }
    }

    value
}

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
    let out = coerce_or_check(value, &schema, "root", &mut errors, coerce);

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

    let example = schema_example(&schema);
    to_js(ctx, &example)
}

fn schema_example(schema: &serde_json::Value) -> serde_json::Value {
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return serde_json::Value::Null,
    };

    // If schema has an explicit example, use it.
    if let Some(ex) = obj.get("example") {
        return ex.clone();
    }

    let type_str = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match type_str {
        "object" => {
            let mut map = serde_json::Map::new();
            if let Some(props) = obj.get("properties").and_then(|p| p.as_object()) {
                for (k, prop_schema) in props {
                    map.insert(k.clone(), schema_example(prop_schema));
                }
            }
            serde_json::Value::Object(map)
        }
        "array" => {
            if let Some(items) = obj.get("items") {
                serde_json::Value::Array(vec![schema_example(items)])
            } else {
                serde_json::Value::Array(vec![])
            }
        }
        "string" => serde_json::Value::String("string".into()),
        "number" => serde_json::Value::Number(serde_json::Number::from(0)),
        "integer" => serde_json::Value::Number(serde_json::Number::from(0)),
        "boolean" => serde_json::Value::Bool(true),
        "null" => serde_json::Value::Null,
        _ => {
            // No type — if properties exist, treat as object.
            if obj.contains_key("properties") {
                return schema_example(
                    &serde_json::json!({"type": "object", "properties": obj.get("properties")}),
                );
            }
            serde_json::Value::Null
        }
    }
}

/// `jsonSchema.matchesType(value, type): boolean`
fn js_json_schema_matchesType(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let value_arg = cc.arg(0);
    let type_arg = cc.arg(1);

    let value: serde_json::Value = from_js(ctx, &value_arg)
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.matchesType: {e}")))?;
    let type_val: serde_json::Value = from_js(ctx, &type_arg)
        .map_err(|e| ctx.throw_type_error(&format!("jsonSchema.matchesType type arg: {e}")))?;

    let ok = matches_type_field(&value, &type_val);
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
