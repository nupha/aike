// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `SimpleSchema` DSL operations — reading tool-argument definitions
//! (the TS `SimpleSchema` / alias `ToolArgDef` interface) and converting
//! them to standard JSON Schema.
//!
//! Takes JS values directly (`Object`) since the DSL only exists on the GC
//! heap; emits `serde_json::Value` because consumers are serde-based
//! (rmcp `Tool::inputSchema` and the `json_schema_*` runtime validators).

use zuqe::{JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK, Object, ScopedContext};

/// Convert DSL `args` object to standard JSON Schema `inputSchema`.
///
/// Input: `{ fieldName: { desc, type?, required?, enum?, default? } }`
/// Output: `{ type: "object", properties: { ... }, required: [...] }`
/// Returns `None` when there are no fields (so callers can fall back to a bare
/// `{ type: "object" }`). Used when building each MCP tool's `inputSchema`
/// for the rmcp protocol layer.
pub(crate) fn simple_schema_from_args(
    ctx: &mut ScopedContext,
    mut args_obj: Object,
) -> Result<Option<serde_json::Value>, i32> {
    let pks = args_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;

    if pks.is_empty() {
        return Ok(None);
    }

    let mut properties = serde_json::Map::new();
    let mut required: Vec<serde_json::Value> = Vec::new();

    for pk in pks.iter() {
        let field_name = match pk.atom.to_value(ctx).and_then(|v| v.to_string(ctx)) {
            Ok(s) => s.content().encode_utf8(false).to_string(),
            Err(_) => {
                continue;
            }
        };
        if field_name.is_empty() {
            continue;
        }

        let Some(field_obj) = args_obj.get_property(ctx, pk.atom)?.as_object() else {
            continue;
        };

        let mut prop = serde_json::Map::new();

        // description (absent/undefined → omit the key)
        if let Some(desc_val) = field_obj
            .get_property_str(ctx, "desc")
            .ok()
            .filter(|v| !v.is_undefined())
            && let Ok(s) = desc_val.to_string(ctx)
        {
            prop.insert(
                "description".into(),
                serde_json::Value::String(s.content().encode_utf8(false).to_string()),
            );
        }

        // type (default: "string" when absent/undefined)
        let type_str = field_obj
            .get_property_str(ctx, "type")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_else(|| "string".into());

        // enum
        let has_enum = field_obj
            .get_property_str(ctx, "enum")
            .ok()
            .filter(|v| !v.is_undefined())
            .is_some();
        if has_enum {
            if let Ok(enum_val) = field_obj.get_property_str(ctx, "enum")
                && !enum_val.is_undefined()
                && let Ok(enum_json) = enum_val.to_serde_json(ctx)
                && !enum_json.is_null()
            {
                prop.insert("enum".into(), enum_json);
            }
        } else if type_str == "array" {
            prop.insert("type".into(), serde_json::Value::String("array".into()));

            // Parse items schema (defaults to { type: "string" })
            let mut items = serde_json::Map::new();
            if let Some(items_obj) = field_obj
                .get_property_str(ctx, "items")
                .ok()
                .filter(|v| !v.is_undefined())
                .and_then(|v| v.as_object())
            {
                let item_type = items_obj
                    .get_property_str(ctx, "type")
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.content().encode_utf8(false).to_string())
                    .unwrap_or_else(|| "string".into());
                items.insert("type".into(), serde_json::Value::String(item_type));

                if let Some(desc) = items_obj
                    .get_property_str(ctx, "desc")
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.content().encode_utf8(false).to_string())
                {
                    items.insert("description".into(), serde_json::Value::String(desc));
                }

                if let Some(enum_val) = items_obj
                    .get_property_str(ctx, "enum")
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .and_then(|v| v.to_serde_json(ctx).ok())
                    .filter(|v| !v.is_null())
                {
                    items.insert("enum".into(), enum_val);
                }
            } else {
                items.insert("type".into(), serde_json::Value::String("string".into()));
            }
            prop.insert("items".into(), serde_json::Value::Object(items));
        } else {
            prop.insert("type".into(), serde_json::Value::String(type_str));
        }

        // default
        if let Ok(default_val) = field_obj.get_property_str(ctx, "default")
            && let Ok(default_json) = default_val.to_serde_json(ctx)
            && !default_json.is_null()
        {
            prop.insert("default".into(), default_json);
        }

        // required
        if let Ok(req_val) = field_obj.get_property_str(ctx, "required")
            && let Ok(req_json) = req_val.to_serde_json(ctx)
            && req_json.as_bool() == Some(true)
        {
            required.push(serde_json::Value::String(field_name.clone()));
        }

        properties.insert(field_name, serde_json::Value::Object(prop));
    }

    if properties.is_empty() {
        return Ok(None);
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), serde_json::Value::String("object".into()));
    schema.insert("properties".into(), serde_json::Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), serde_json::Value::Array(required));
    }

    Ok(Some(serde_json::Value::Object(schema)))
}
