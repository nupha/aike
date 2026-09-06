// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com)

//! `jinja2` module — General-purpose jinja2 template rendering (via minijinja).
//!
//! Exposed as `import jinja2 from "jinja2"`.

use minijinja::Environment;
use minijinja_contrib::pycompat;
use zuqe::{CallContext, FieldDef, JSResult, JSValue, ScopedContext};

/// Create a minimal jinja2 environment with Python-compatible syntax.
///
/// Includes only minijinja built-ins (lower, upper, tojson, range, etc.).
/// Does NOT include llama.cpp-specific functions like `raise_exception`,
/// `strftime_now`, or `rsplit`.
pub fn create_minijinja_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_unknown_method_callback(pycompat::unknown_method_callback);
    env
}

/// Render a jinja2 template string and return the result.
///
/// # Arguments
/// * `template` - jinja2 template string
/// * `variables` - Template variables (JSON object)
///
/// # Returns
/// Rendered string, or error message
pub fn render_template(
    template: &str,
    variables: Option<&serde_json::Value>,
) -> Result<String, String> {
    let mut env = create_minijinja_env();

    env.add_template("template", template)
        .map_err(|e| format!("template parse error: {e}"))?;

    let tmpl = env
        .get_template("template")
        .map_err(|e| format!("template get error: {e}"))?;

    let ctx = match variables {
        Some(v) => minijinja::Value::from_serialize(v),
        None => minijinja::Value::from(()),
    };

    tmpl.render(ctx)
        .map_err(|e| format!("template render error: {e}"))
}

// ---------------------------------------------------------------------------
// JS bindings
// ---------------------------------------------------------------------------

/// `renderTemplate(template: string, variables?: object): string`
fn js_render_template(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let template_v = cc.arg(0).to_string(ctx)?;
    let template = template_v.content().encode_utf8(false).to_string();

    let variables = if cc.arg(1).is_undefined() {
        None
    } else {
        let val = cc.arg(1).to_serde_json(ctx).map_err(|e| {
            ctx.throw_type_error(&format!("variables must be convertible to JSON: {e}"))
        })?;
        Some(val)
    };

    let result =
        render_template(&template, variables.as_ref()).map_err(|e| ctx.throw_type_error(&e))?;

    ctx.create_string_from_str(&result)
        .map(JSValue::make_string)
}

const MOD_JINJA2_ENTRIES: &[FieldDef] = &[FieldDef::OBJECT(
    "default",
    &[FieldDef::FUNC("renderTemplate", 2, js_render_template, 0)],
    zuqe::JS_PROP_ENUMERABLE,
)];

pub fn init_mod_jinja2(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("jinja2", MOD_JINJA2_ENTRIES)?;
    Ok(())
}
