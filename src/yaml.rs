// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use zuqe::{CallContext, FieldDef, JSResult, JSValue, ModuleDef, ScopedContext};

/// YAML.parse(text) — parse YAML string into a JS value
fn js_yaml_parse(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let text = v.content().encode_utf8(false).to_string();

    if text.trim().is_empty() {
        return Ok(JSValue::UNDEFINED);
    }

    let val: serde_json::Value = serde_yaml::from_str(&text)
        .map_err(|e| ctx.throw_syntax_error(&format!("YAML parse error: {e}")))?;

    JSValue::from_serde_json(ctx, &val)
}

/// YAML.stringify(value) — serialize a JS value to YAML string
fn js_yaml_stringify(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let arg = cc.arg(0);
    if arg.is_undefined() || arg.is_callable() {
        return Err(ctx.throw_type_error("YAML.stringify: cannot serialize this value"));
    }

    let val = arg
        .to_serde_json(ctx)
        .map_err(|_| ctx.throw_type_error("YAML.stringify: cannot serialize value"))?;

    let yaml_str = serde_yaml::to_string(&val)
        .map_err(|e| ctx.throw_internal_error(&format!("YAML stringify error: {e}")))?;

    ctx.create_string_from_utf8(yaml_str.into_bytes())
        .map(JSValue::make_string)
}

const YAML_FUNCS: &[FieldDef] = &[
    FieldDef::FUNC("parse", 1, js_yaml_parse, 0),
    FieldDef::FUNC("stringify", 1, js_yaml_stringify, 0),
];

const MOD_YAML_ENTRIES: &[FieldDef] = &[FieldDef::OBJECT(
    "YAML",
    YAML_FUNCS,
    zuqe::JS_PROP_ENUMERABLE,
)];

pub fn install_mod_yaml(ctx: &mut ScopedContext) -> Result<ModuleDef, i32> {
    ctx.create_module("yaml", MOD_YAML_ENTRIES)
}
