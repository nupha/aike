// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com)

//! `jinja2` module — General-purpose jinja2 template rendering (via minijinja).
//!
//! Exposed as:
//!   - `import jinja2 from "jinja2"` — standalone renderTemplate()
//!   - `import { Jinja2 } from "jinja2"` — class with JS callback support

use std::{cell::RefCell, collections::HashMap};

use linkme::distributed_slice;
use minijinja::{Environment, value::Rest};
use minijinja_contrib::pycompat;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, GcTrace, JSResult, JSValue, NativeObject,
    Runtime, ScopedContext, def_native_object, gc_lite::GcTraceCtx,
};

use crate::USER_CLASS_DEFS;

// ---------------------------------------------------------------------------
// Thread-local storage for JS callbacks and context during render
// ---------------------------------------------------------------------------

thread_local! {
    static JS_CALLBACKS: RefCell<HashMap<String, JSValue>> = RefCell::new(HashMap::new());
    static JS_CTX: RefCell<Option<*mut ScopedContext<'static>>> = RefCell::new(None);
}

pub(crate) fn get_callback(name: &str) -> Option<JSValue> {
    JS_CALLBACKS.with(|c| c.borrow().get(name).cloned())
}

/// Get current ScopedContext from thread-local (only valid during render)
pub(crate) fn with_ctx<R>(f: impl FnOnce(&mut ScopedContext) -> R) -> Option<R> {
    JS_CTX.with(|c| {
        let ptr = c.borrow();
        if let Some(ptr) = *ptr {
            // SAFETY: ptr is valid during render, cleared after
            let ctx = unsafe { &mut *ptr };
            Some(f(ctx))
        } else {
            None
        }
    })
}

/// RAII guard: registers callbacks and context, clears on drop
struct CallbackGuard;

impl CallbackGuard {
    fn new(ctx: &mut ScopedContext, payload: &Jinja2Payload) -> Self {
        JS_CALLBACKS.with(|c| {
            *c.borrow_mut() = payload.functions.clone();
        });
        JS_CTX.with(|c| {
            *c.borrow_mut() = Some(ctx as *mut ScopedContext as *mut ScopedContext<'static>);
        });
        CallbackGuard
    }
}

impl Drop for CallbackGuard {
    fn drop(&mut self) {
        JS_CALLBACKS.with(|c| c.borrow_mut().clear());
        JS_CTX.with(|c| {
            *c.borrow_mut() = None;
        });
    }
}

// ---------------------------------------------------------------------------
// Jinja2 class
// ---------------------------------------------------------------------------

pub(crate) struct Jinja2Payload {
    functions: HashMap<String, JSValue>,
}

impl GcTrace for Jinja2Payload {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        for v in self.functions.values() {
            JSValue::from(v.clone()).trace(gcx);
        }
    }
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let mut functions = HashMap::new();

    if cc.length() > 0 {
        let opts = cc.arg(0);
        if !opts.is_undefined() {
            let obj = opts
                .as_object()
                .ok_or_else(|| ctx.throw_type_error("options must be an object"))?;

            if let Some(funcs_val) = obj
                .get_property_str(ctx, "functions")
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_null())
            {
                if let Some(mut funcs_obj) = funcs_val.as_object() {
                    if let Ok(keys) = funcs_obj.get_own_property_names(
                        ctx,
                        zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK,
                    ) {
                        for pk in keys.to_vec() {
                            let key_str = if let Some(i) = pk.atom.to_small_int() {
                                format!("{i}")
                            } else if let Ok(kv) = pk.atom.to_value(ctx) {
                                kv.to_string(ctx)?.content().encode_utf8(false).to_string()
                            } else {
                                continue;
                            };

                            let val = funcs_obj.get_property_str(ctx, &key_str)?;
                            if val.is_callable() {
                                functions.insert(key_str, val);
                            }
                        }
                    }
                }
            }
        }
    }

    let payload = Jinja2Payload { functions };
    JSJinja2::new_instance(ctx, new_target, payload).map(JSValue::from)
}

fn js_render(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload =
        JSJinja2::payload_of(&this).ok_or_else(|| ctx.throw_type_error("not a Jinja2 instance"))?;

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

    // RAII guard: registers callbacks and context, ensures cleanup
    let _guard = CallbackGuard::new(ctx, &payload);

    let mut env = create_minijinja_env();

    // Register callback function wrappers
    for name in payload.functions.keys() {
        let name_clone = name.clone();
        env.add_function(
            name.clone(),
            move |args: Rest<minijinja::Value>| -> Result<minijinja::Value, minijinja::Error> {
                let args = args.0;

                // Get callback from thread-local
                let func = get_callback(&name_clone).ok_or_else(|| {
                    minijinja::Error::new(
                        minijinja::ErrorKind::InvalidOperation,
                        format!("JS callback '{}' not found", name_clone),
                    )
                })?;

                // Get context from thread-local and call JS function
                with_ctx(|ctx| {
                    // Convert minijinja args to JSValue
                    let js_args: Vec<JSValue> = args
                        .iter()
                        .map(|v| {
                            let s = v.to_string();
                            ctx.create_string_from_str(&s)
                                .map(JSValue::make_string)
                                .unwrap_or(JSValue::UNDEFINED)
                        })
                        .collect();

                    // Call JS function
                    let result = func.call(ctx, JSValue::UNDEFINED, &js_args).map_err(|_| {
                        let exc = ctx.take_exception_as_text();
                        minijinja::Error::new(
                            minijinja::ErrorKind::InvalidOperation,
                            format!("JS callback '{}' error: {}", name_clone, exc.message),
                        )
                    })?;

                    // Convert result to minijinja Value
                    let result_str = result
                        .to_string(ctx)
                        .map(|s| s.content().encode_utf8(false).to_string())
                        .unwrap_or_default();

                    Ok(minijinja::Value::from(result_str))
                })
                .unwrap_or_else(|| {
                    Err(minijinja::Error::new(
                        minijinja::ErrorKind::InvalidOperation,
                        format!("JS callback '{}' called outside render context", name_clone),
                    ))
                })
            },
        );
    }

    env.add_template("template", &template)
        .map_err(|e| ctx.throw_type_error(&format!("template parse error: {e}")))?;

    let tmpl = env
        .get_template("template")
        .map_err(|e| ctx.throw_type_error(&format!("template get error: {e}")))?;

    let ctx_val = match variables {
        Some(v) => minijinja::Value::from_serialize(&v),
        None => minijinja::Value::from(()),
    };

    let result = tmpl
        .render(ctx_val)
        .map_err(|e| ctx.throw_type_error(&format!("template render error: {e}")))?;

    ctx.create_string_from_str(&result)
        .map(JSValue::make_string)
}

const CLASS_JINJA2_PROTO: &[FieldDef] = &[FieldDef::FUNC("render", 2, js_render, 0)];

def_native_object!(
    "Jinja2",
    JSJinja2,
    Jinja2Payload,
    CLASS_JINJA2_PROTO,
    Some(ClassCtorDef::new(js_constructor, 0, true))
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_jinja2(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSJinja2::register(rt)
}

// ---------------------------------------------------------------------------
// Standalone renderTemplate
// ---------------------------------------------------------------------------

pub fn create_minijinja_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_unknown_method_callback(pycompat::unknown_method_callback);
    env
}

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

const MOD_JINJA2_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        JSJinja2::CLASS_NAME,
        JSJinja2::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    ),
    FieldDef::OBJECT(
        "default",
        &[FieldDef::FUNC("renderTemplate", 2, js_render_template, 0)],
        zuqe::JS_PROP_ENUMERABLE,
    ),
];

pub fn init_mod_jinja2(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.use_classes(&[JSJinja2::class_id()])?;
    ctx.create_module("jinja2", MOD_JINJA2_ENTRIES)?;
    Ok(())
}
