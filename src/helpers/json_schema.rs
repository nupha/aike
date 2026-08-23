// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Shared JSON-Schema operations — the single home for schema type matching,
//! recursive validation/coercion, example synthesis, and MCP tool-argument
//! checking. Both the `json-schema` JS module and the MCP server consume
//! these instead of keeping private copies.
//!
//! Two `integer` contracts coexist on purpose (frozen behavior per caller):
//! - lenient ([`json_schema_type_matches`]): JS `jsonSchema` API; accepts
//!   i64/u64 and whole-number floats like `2.0`.
//! - strict ([`json_schema_type_strict`] / [`json_schema_item_type_strict`]):
//!   MCP tool-arg validation; only i64-representable values pass.

// ── type matching ──

/// Check if a JSON value matches a JSON-Schema type keyword (lenient contract).
pub(crate) fn json_schema_type_matches(value: &serde_json::Value, schema_type: &str) -> bool {
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

/// Type match under the MCP tool-args contract: `integer` accepts only values
/// representable as i64 (whole floats like `2.0` and huge u64 are rejected).
/// Other keywords follow [`json_schema_type_matches`].
pub(crate) fn json_schema_type_strict(value: &serde_json::Value, schema_type: &str) -> bool {
    match schema_type {
        "integer" => value.as_i64().is_some(),
        _ => json_schema_type_matches(value, schema_type),
    }
}

/// Element-type check inside an array's `items`, under the MCP tool-args
/// contract. Historical quirk preserved: no `array` arm — an item typed
/// `array` accepts any element.
pub(crate) fn json_schema_item_type_strict(value: &serde_json::Value, item_type: &str) -> bool {
    match item_type {
        "integer" => value.as_i64().is_some(),
        "array" => true,
        _ => json_schema_type_matches(value, item_type),
    }
}

/// Check if a value matches a type which may be a string or array of strings.
pub(crate) fn json_schema_matches_type_field(
    value: &serde_json::Value,
    type_field: &serde_json::Value,
) -> bool {
    match type_field {
        serde_json::Value::String(s) => json_schema_type_matches(value, s),
        serde_json::Value::Array(arr) => arr.iter().any(|t| {
            if let serde_json::Value::String(s) = t {
                json_schema_type_matches(value, s)
            } else {
                false
            }
        }),
        _ => true, // no type specified → accept
    }
}

// ── recursive validation / coercion ──

/// Recursively validate a value against a JSON-Schema, optionally coercing.
/// Returns the (possibly coerced) value and collects errors.
///
/// `changed` is set to true iff at least one coercion produced a new value;
/// callers use it to skip re-emitting an unchanged value.
pub(crate) fn json_schema_coerce_or_check(
    value: serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
    coerce: bool,
    changed: &mut bool,
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
                    let coerced = json_schema_coerce_or_check(
                        v.clone(),
                        prop_schema,
                        &format!("{path}.{k}"),
                        errors,
                        coerce,
                        changed,
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
                    json_schema_coerce_or_check(
                        v, items_schema, &format!("{path}[{i}]"), errors, coerce, changed,
                    )
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
                    if !json_schema_type_matches(&value, "string") {
                        if coerce && (value.is_number() || value.is_boolean()) {
                            let s = match &value {
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::Bool(b) => b.to_string(),
                                _ => String::new(),
                            };
                            *changed = true;
                            return serde_json::Value::String(s);
                        }
                        errors.push(format!("{path}: expected string"));
                    }
                    return value;
                }
                "number" | "integer" => {
                    if !json_schema_type_matches(&value, type_str) {
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
                                    *changed = true;
                                    return serde_json::Value::Number(num);
                                }
                            }
                        }
                        errors.push(format!("{path}: expected {type_str}"));
                        return value;
                    }
                    if type_str == "integer" && !json_schema_type_matches(&value, "integer") {
                        errors.push(format!("{path}: expected integer"));
                    }
                    return value;
                }
                "boolean" => {
                    if !json_schema_type_matches(&value, "boolean") {
                        if coerce {
                            match &value {
                                serde_json::Value::String(s) if s == "true" => {
                                    *changed = true;
                                    return serde_json::Value::Bool(true);
                                }
                                serde_json::Value::String(s) if s == "false" => {
                                    *changed = true;
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

// ── example synthesis ──

/// Synthesize an example value from a JSON-Schema.
pub(crate) fn json_schema_example(schema: &serde_json::Value) -> serde_json::Value {
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
                    map.insert(k.clone(), json_schema_example(prop_schema));
                }
            }
            serde_json::Value::Object(map)
        }
        "array" => {
            if let Some(items) = obj.get("items") {
                serde_json::Value::Array(vec![json_schema_example(items)])
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
                return json_schema_example(
                    &serde_json::json!({"type": "object", "properties": obj.get("properties")}),
                );
            }
            serde_json::Value::Null
        }
    }
}

// ── MCP tool-argument checking ──
//
// Coverage (current):
//   required  — missing required field rejected
//   enum      — whitelist check
//   type      — string / integer / number / boolean / object / null / array
//               (incl. array `items` element type); unknown type keywords pass
//   default   — missing optional args filled by [`json_schema_apply_defaults`],
//               then the inserted value is validated like any provided value
//
// Not yet implemented (standard JSON Schema "Validation" vocabulary; the MCP
// DSL `SimpleSchema` (alias `ToolArgDef`) does not expose them yet, so they
// never appear in the schemas `simple_schema_from_args` produces):
//   minLength/maxLength (string, Unicode code points)
//   pattern             (string must match a regex)
//   minimum/maximum / exclusiveMinimum/exclusiveMaximum (numeric bounds)
//   multipleOf          (number must be a multiple)
//   minItems/maxItems, uniqueItems (array)
//   minProperties/maxProperties (object)
//   format              (semantic hint: email/date-time/uri/ipv4… — in
//                       2020-12 it is Format-Annotation, default non-enforcing)
// When the DSL is extended, propagate the new fields in
// `simple_schema_from_args` (helpers/simple_schema.rs) and add checks here.
// Schema matches `memo/mcp-schema-validation.md`.

/// Validate MCP tool-call `arguments` against a tool's `inputSchema` before
/// dispatch.
///
/// Covers the subset of JSON Schema that `simple_schema_from_args`
/// produces: an object with `required` and per-property `type`/`enum`
/// (including array `items`). Types string/integer/number/boolean/object/
/// null/array are enforced; extra keys are ignored; unknown types pass.
pub(crate) fn json_schema_validate_args(
    schema: &serde_json::Map<String, serde_json::Value>,
    arguments: &serde_json::Value,
) -> Result<(), String> {
    let Some(obj) = arguments.as_object() else {
        return Err("arguments must be a JSON object".to_string());
    };

    // required fields
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for name in required {
            let Some(name) = name.as_str() else { continue };
            if !obj.contains_key(name) {
                return Err(format!("missing required argument: \"{}\"", name));
            }
        }
    }

    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return Ok(());
    };
    for (prop_name, prop_schema) in properties {
        if let Some(value) = obj.get(prop_name) {
            json_schema_validate_property(prop_name, prop_schema, value)?;
        }
    }

    Ok(())
}

/// Fill missing optional MCP tool arguments with their schema `default` values.
///
/// A non-required property that carries a `default` is inserted into
/// `arguments` when absent, so handlers always receive the default. If
/// `arguments` is not an object (e.g. `null` when no args were sent) it is
/// replaced by a fresh object whenever at least one default exists.
pub(crate) fn json_schema_apply_defaults(
    schema: &serde_json::Map<String, serde_json::Value>,
    arguments: &mut serde_json::Value,
) {
    let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) else {
        return;
    };
    // Fast path: no property carries a default.
    if !properties.values().any(|p| p.get("default").is_some()) {
        return;
    }

    let obj = match arguments.as_object_mut() {
        Some(obj) => obj,
        None => {
            *arguments = serde_json::Value::Object(serde_json::Map::new());
            arguments.as_object_mut().expect("just created object")
        }
    };
    for (name, prop_schema) in properties {
        if !obj.contains_key(name) {
            if let Some(default) = prop_schema.get("default") {
                obj.insert(name.clone(), default.clone());
            }
        }
    }
}

/// Validate one property value against its property schema (MCP contract).
///
/// Enum is checked before type; error wording is part of the MCP protocol
/// surface and must not change.
fn json_schema_validate_property(
    name: &str,
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), String> {
    // enum whitelist
    if let Some(list) = schema.get("enum").and_then(|v| v.as_array()) {
        if !list.iter().any(|v| v == value) {
            return Err(format!(
                "invalid value for \"{}\": {}",
                name,
                serde_json::to_string(value).unwrap_or_default()
            ));
        }
    }

    match schema.get("type").and_then(|v| v.as_str()) {
        None => Ok(()),
        Some("string") => {
            if json_schema_type_strict(value, "string") {
                Ok(())
            } else {
                Err(format!("\"{}\" must be a string", name))
            }
        }
        Some("integer") => {
            if json_schema_type_strict(value, "integer") {
                Ok(())
            } else {
                Err(format!("\"{}\" must be an integer", name))
            }
        }
        Some("boolean") => {
            if json_schema_type_strict(value, "boolean") {
                Ok(())
            } else {
                Err(format!("\"{}\" must be a boolean", name))
            }
        }
        Some("number") => {
            if json_schema_type_strict(value, "number") {
                Ok(())
            } else {
                Err(format!("\"{}\" must be a number", name))
            }
        }
        Some("object") => {
            if json_schema_type_strict(value, "object") {
                Ok(())
            } else {
                Err(format!("\"{}\" must be an object", name))
            }
        }
        Some("null") => {
            if json_schema_type_strict(value, "null") {
                Ok(())
            } else {
                Err(format!("\"{}\" must be null", name))
            }
        }
        Some("array") => {
            let Some(items) = value.as_array() else {
                return Err(format!("\"{}\" must be an array", name));
            };
            // Validate each item against the item type (default: string)
            let item_type = schema
                .get("items")
                .and_then(|i| i.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("string");
            for item in items {
                if !json_schema_item_type_strict(item, item_type) {
                    return Err(format!("items of \"{}\" must be {}", name, item_type));
                }
            }
            Ok(())
        }
        // Unknown type keyword — don't enforce.
        Some(_) => Ok(()),
    }
}
