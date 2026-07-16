// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Native implementation of the `MCPServerDef` builder class and the
//! `defineMCPStdio` factory, ported from `ravenclaw/src/mcp-server-def.ts`.
//!
//! `MCPServerDef` collects a declarative MCP stdio server definition via a
//! chainable builder API (`version`, `description`, `shared`, `args`, `env`,
//! `tool`, `extraFiles`, `beforeRun`). `info()` returns the metadata without
//! side effects; `run()` starts the server via the native `McpStdio` class.

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
};

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JS_GPN_ENUM_ONLY,
    JS_GPN_STR_OR_INT_MASK, JS_PROP_C_W_E, JSResult, JSValue, NativeObject, Object, Runtime,
    ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, http::JSMcpStdio};

/// Environment entry value: a static string, or a JS function resolving to
/// `string | undefined`.
enum EnvVal {
    Static(String),
    Func(Object),
}

/// args entry: a static string array, or a JS function resolving to `string[]`.
enum ArgsVal {
    Static(Vec<String>),
    Func(Object),
}

/// Per-tool definition: description, JS handler function, and optional args schema.
#[derive(Clone)]
struct McpToolDef {
    desc: String,
    handler: Object,
    args: Option<Object>,
}

/// Payload stored on every `MCPServerDef` instance.
pub(crate) struct MCPServerDefPayload {
    server_name: String,
    version: RefCell<Cow<'static, str>>,
    description: RefCell<Cow<'static, str>>,
    shared: Cell<bool>,
    extra_files: RefCell<Vec<String>>,
    tools: RefCell<HashMap<String, McpToolDef>>,
    env_entries: RefCell<Vec<(String, EnvVal)>>,
    args_entry: RefCell<Option<ArgsVal>>,
    before_run: RefCell<Option<Object>>,
}

impl MCPServerDefPayload {
    fn new(server_name: String) -> Self {
        Self {
            server_name,
            version: RefCell::new(Cow::Borrowed("1.0.0")),
            description: RefCell::new(Cow::Borrowed("")),
            shared: Cell::new(false),
            extra_files: RefCell::new(Vec::new()),
            tools: RefCell::new(HashMap::new()),
            env_entries: RefCell::new(Vec::new()),
            args_entry: RefCell::new(None),
            before_run: RefCell::new(None),
        }
    }

    /// GC trace — keeps referenced JS objects (handlers, callbacks) alive.
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Ok(tools) = self.tools.try_borrow() {
            for t in tools.values() {
                t.handler.trace(gcx);
                if let Some(a) = &t.args {
                    a.trace(gcx);
                }
            }
        }
        if let Ok(envs) = self.env_entries.try_borrow() {
            for (_, v) in envs.iter() {
                if let EnvVal::Func(f) = v {
                    f.trace(gcx);
                }
            }
        }
        if let Ok(a) = self.args_entry.try_borrow() {
            if let Some(ArgsVal::Func(f)) = a.as_ref() {
                f.trace(gcx);
            }
        }
        if let Ok(br) = self.before_run.try_borrow() {
            if let Some(f) = br.as_ref() {
                f.trace(gcx);
            }
        }
    }
}

fn get_def<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a MCPServerDefPayload, i32> {
    val.as_native_object::<JSMCPServerDef>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not an MCPServerDef instance");
            -1
        })
}

/// Create a JS string value, taking ownership of `s` so no extra allocation
/// is needed when the caller already holds a `String`.
fn js_str(ctx: &mut ScopedContext, s: String) -> JSResult {
    Ok(ctx.create_string_from_utf8(s.into_bytes())?.into())
}

// ── Constructor ──

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    JSMCPServerDef::new_instance(ctx, cc.this(), MCPServerDefPayload::new(name)).map(JSValue::from)
}

// ── Chain methods ──

/// Magic consts distinguishing which string field `js_set_string` writes.
const SET_VERSION: i16 = 0;
const SET_DESCRIPTION: i16 = 1;

/// Shared setter for `version` / `description` — the target field is
/// selected via the function's magic const.
fn js_set_string(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let v = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    match cc.magic() as i16 {
        SET_VERSION => *p.version.borrow_mut() = Cow::Owned(v),
        _ => *p.description.borrow_mut() = Cow::Owned(v),
    }
    Ok(this)
}

fn js_shared(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    p.shared.set(cc.arg(0).to_bool().unwrap_or(false));
    Ok(this)
}

fn js_args(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let v = cc.arg(0);
    let is_func = v.as_object().map_or(false, |o| o.is_callable());
    let val = if is_func {
        ArgsVal::Func(v.into_callable(ctx)?)
    } else {
        // args must be a string[] — take ownership so we can mutate it.
        let arr = v.into_object(ctx)?;
        let len = arr
            .get_property_str(ctx, "length")
            .and_then(|l| l.to_number(ctx))
            .map(|n| n.to_i32())? as usize;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let s = arr
                .get_property_u32(ctx, i as u32)?
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
            out.push(s);
        }
        ArgsVal::Static(out)
    };
    *p.args_entry.borrow_mut() = Some(val);
    Ok(this)
}

fn js_env(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let v = cc.arg(1);
    let is_func = v.as_object().map_or(false, |o| o.is_callable());
    let val = if is_func {
        EnvVal::Func(v.into_callable(ctx)?)
    } else {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        EnvVal::Static(s)
    };
    p.env_entries.borrow_mut().push((name, val));
    Ok(this)
}

fn js_tool(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let def = cc
        .arg(1)
        .as_object()
        .ok_or_else(|| {
            ctx.throw_type_error("tool def must be an object");
            -1
        })?
        .clone();
    let desc = def
        .get_property_str(ctx, "desc")
        .and_then(|v| v.to_string(ctx))
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let handler = def
        .get_property_str(ctx, "handler")
        .and_then(|v| v.into_callable(ctx))
        .map_err(|_| {
            ctx.throw_type_error(&format!("tool '{}' handler must be a function", name));
            -1
        })?;
    let args = def
        .get_property_str(ctx, "args")
        .ok()
        .filter(|v| !v.is_undefined())
        .and_then(|v| v.as_object().copied());
    p.tools.borrow_mut().insert(
        name,
        McpToolDef {
            desc,
            handler,
            args: args.clone(),
        },
    );
    Ok(this)
}

fn js_extra_files(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let n = cc.length();
    let mut paths = Vec::with_capacity(n as usize);
    for i in 0..n {
        let s = cc
            .arg(i)
            .to_string(ctx)?
            .content()
            .encode_utf8(false)
            .to_string();
        paths.push(s);
    }
    *p.extra_files.borrow_mut() = paths;
    Ok(this)
}

fn js_before_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let func = cc.arg(0).into_callable(ctx)?;
    *p.before_run.borrow_mut() = Some(func);
    Ok(this)
}

// ── Metadata (no side effects) ──

/// Build the JSON-schema `inputSchema` for a tool's `args` DSL object,
/// mirroring the TS `dslToJsonSchema`.
fn dsl_to_json_schema(mut ctx: &mut ScopedContext, mut args_obj: Object) -> JSResult {
    let pks = args_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    let mut properties = ctx.create_base_object()?;
    let mut required: Vec<String> = Vec::new();

    for pk in pks.iter() {
        let name = pk
            .atom
            .to_value(ctx)
            .and_then(|v| v.to_string(ctx))
            .map(|s| s.content().encode_utf8(false).to_string())
            .map_err(|_| {
                ctx.throw_type_error("tool arg name must be a string");
                -1
            })?;
        if name.is_empty() {
            continue;
        }
        let prop_val = args_obj.get_property(ctx, pk.atom)?;
        let prop_obj = match prop_val.as_object() {
            Some(o) => o.clone(),
            None => ctx.create_base_object()?,
        };

        let mut prop = ctx.create_base_object()?;
        let desc = prop_obj
            .get_property_str(ctx, "desc")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();
        let desc_val = js_str(ctx, desc)?;
        prop.define_property_value_str(ctx, "description", desc_val, JS_PROP_C_W_E)?;

        let type_str = prop_obj
            .get_property_str(ctx, "type")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string());
        if let Some(ts) = &type_str {
            if ts != "string" {
                let type_val = js_str(ctx, ts.clone())?;
                prop.define_property_value_str(ctx, "type", type_val, JS_PROP_C_W_E)?;
                if ts == "array" {
                    if let Some(items) = prop_obj
                        .get_property_str(ctx, "items")
                        .ok()
                        .filter(|v| !v.is_undefined())
                        .and_then(|v| v.as_object().copied())
                    {
                        prop.define_property_value_str(ctx, "items", items.into(), JS_PROP_C_W_E)?;
                    }
                }
            }
        }

        if let Some(enum_val) = prop_obj
            .get_property_str(ctx, "enum")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_serde_json(ctx).ok())
            .filter(|v| !v.is_null())
        {
            let enum_js = JSValue::from_serde_json(ctx, &enum_val)?;
            prop.define_property_value_str(ctx, "enum", enum_js, JS_PROP_C_W_E)?;
        }

        if let Some(default_val) = prop_obj.get_property_str(ctx, "default").ok() {
            if let Ok(dj) = default_val.to_serde_json(ctx) {
                if !dj.is_null() {
                    let default_js = JSValue::from_serde_json(ctx, &dj)?;
                    prop.define_property_value_str(ctx, "default", default_js, JS_PROP_C_W_E)?;
                }
            }
        }

        if let Ok(req_val) = prop_obj.get_property_str(ctx, "required") {
            if let Ok(req_json) = req_val.to_serde_json(ctx) {
                if req_json.as_bool() == Some(true) {
                    required.push(name.clone());
                }
            }
        }

        properties.define_property_value_str(ctx, &name, prop.into(), JS_PROP_C_W_E)?;
    }

    let mut schema = ctx.create_base_object()?;
    let type_val = js_str(ctx, "object".to_string())?;
    schema.define_property_value_str(ctx, "type", type_val, JS_PROP_C_W_E)?;
    schema.define_property_value_str(ctx, "properties", properties.into(), JS_PROP_C_W_E)?;
    if !required.is_empty() {
        let mut req_arr = Array::create(ctx)?;
        for (i, n) in required.iter().enumerate() {
            let v = js_str(ctx, n.clone())?;
            req_arr.define_property_value_u32(ctx, i as u32, v, JS_PROP_C_W_E)?;
        }
        schema.define_property_value_str(ctx, "required", req_arr.into(), JS_PROP_C_W_E)?;
    }
    Ok(schema.into())
}

fn js_info(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    let ctx_arg = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0)
    } else {
        JSValue::UNDEFINED
    };

    // env
    let env_empty = p.env_entries.borrow().is_empty();
    let mut env_obj = ctx.create_base_object()?;
    for (name, val) in p.env_entries.borrow().iter() {
        let resolved: Option<String> = match val {
            EnvVal::Static(s) => Some(s.clone()),
            EnvVal::Func(f) => {
                let r = f.call(ctx, JSValue::UNDEFINED, &[ctx_arg])?;
                if r.is_undefined() {
                    None
                } else {
                    Some(r.to_string(ctx)?.content().encode_utf8(false).to_string())
                }
            }
        };
        if let Some(s) = resolved {
            let v = js_str(ctx, s)?;
            env_obj.define_property_value_str(ctx, name, v, JS_PROP_C_W_E)?;
        }
    }

    // args
    let args_val: JSValue = match p.args_entry.borrow().as_ref() {
        Some(ArgsVal::Static(a)) => {
            let mut arr = Array::create(ctx)?;
            for (i, s) in a.iter().enumerate() {
                let v = js_str(ctx, s.clone())?;
                arr.define_property_value_u32(ctx, i as u32, v, JS_PROP_C_W_E)?;
            }
            arr.into()
        }
        Some(ArgsVal::Func(f)) => {
            let r = f.call(ctx, JSValue::UNDEFINED, &[ctx_arg])?;
            if r.is_undefined() {
                JSValue::UNDEFINED
            } else {
                r
            }
        }
        None => JSValue::UNDEFINED,
    };

    // tools
    let mut tools_arr = Array::create(ctx)?;
    for (i, (name, t)) in p.tools.borrow().iter().enumerate() {
        let mut meta = ctx.create_base_object()?;
        let name_val = js_str(ctx, name.clone())?;
        meta.define_property_value_str(ctx, "name", name_val, JS_PROP_C_W_E)?;
        let desc_val = js_str(ctx, t.desc.clone())?;
        meta.define_property_value_str(ctx, "description", desc_val, JS_PROP_C_W_E)?;
        let schema = if let Some(args_obj) = t.args {
            dsl_to_json_schema(ctx, args_obj)?
        } else {
            let mut empty = ctx.create_base_object()?;
            let type_val = js_str(ctx, "object".to_string())?;
            empty.define_property_value_str(ctx, "type", type_val, JS_PROP_C_W_E)?;
            empty.into()
        };
        meta.define_property_value_str(ctx, "inputSchema", schema, JS_PROP_C_W_E)?;
        tools_arr.define_property_value_u32(ctx, i as u32, meta.into(), JS_PROP_C_W_E)?;
    }

    let server_name = p.server_name.clone();
    let description = p.description.borrow().clone();
    let description = description.as_ref();
    let shared = p.shared.get();

    let mut info = ctx.create_base_object()?;
    let sn = js_str(ctx, server_name)?;
    info.define_property_value_str(ctx, "serverName", sn, JS_PROP_C_W_E)?;
    let desc = js_str(ctx, description.to_string())?;
    info.define_property_value_str(ctx, "description", desc, JS_PROP_C_W_E)?;
    info.define_property_value_str(ctx, "shared", JSValue::from(shared), JS_PROP_C_W_E)?;
    info.define_property_value_str(
        ctx,
        "env",
        if env_empty {
            JSValue::UNDEFINED
        } else {
            env_obj.into()
        },
        JS_PROP_C_W_E,
    )?;
    info.define_property_value_str(ctx, "args", args_val, JS_PROP_C_W_E)?;
    info.define_property_value_str(ctx, "tools", tools_arr.into(), JS_PROP_C_W_E)?;
    Ok(info.into())
}

// ── Server entry point ──

fn js_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let before_run = get_def(ctx, &this)?.before_run.borrow().clone();

    if let Some(func) = before_run {
        let r = func.call(ctx, JSValue::UNDEFINED, &[])?;
        if let Ok(mut p) = Promise::try_from(r) {
            // After beforeRun resolves (or errors), start the server.
            let start_fn = ctx.create_native_function_with_data(
                Callback::Func(js_run_do_start),
                "run",
                0,
                0,
                [this].into(),
            )?;
            let fail_fn = ctx.create_native_function_with_data(
                Callback::Func(js_run_ignore_err),
                "",
                1,
                0,
                [this].into(),
            )?;
            return p.then(ctx, start_fn.into(), fail_fn.into()).map(Into::into);
        }
        // beforeRun returned a non-promise value: start immediately.
    }
    run_start(ctx, this)
}

fn js_run_do_start(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.data(0);
    run_start(ctx, this)
}

/// Ignore beforeRun errors and still start the server.
fn js_run_ignore_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.data(0);
    run_start(ctx, this)
}

/// Build the options object and start the stdio MCP server, then register
/// signal handlers that stop it and force-exit once the server stops.
fn run_start(ctx: &mut ScopedContext, this: JSValue) -> JSResult {
    let (server_name, version, tools) = {
        let p = get_def(ctx, &this)?;
        let tools: Vec<(String, McpToolDef)> = p
            .tools
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        (p.server_name.clone(), p.version.borrow().clone(), tools)
    };
    let version = version.as_ref();

    let mut options = ctx.create_base_object()?;
    let sn = js_str(ctx, server_name)?;
    options.define_property_value_str(ctx, "serverName", sn, JS_PROP_C_W_E)?;
    let ver = js_str(ctx, version.to_string())?;
    options.define_property_value_str(ctx, "version", ver, JS_PROP_C_W_E)?;

    let mut tools_obj = ctx.create_base_object()?;
    for (name, t) in &tools {
        let mut t_obj = ctx.create_base_object()?;
        let desc = js_str(ctx, t.desc.clone())?;
        t_obj.define_property_value_str(ctx, "desc", desc, JS_PROP_C_W_E)?;
        t_obj.define_property_value_str(ctx, "handler", t.handler.into(), JS_PROP_C_W_E)?;
        if let Some(args) = t.args {
            t_obj.define_property_value_str(ctx, "args", args.into(), JS_PROP_C_W_E)?;
        }
        tools_obj.define_property_value_str(ctx, &name, t_obj.into(), JS_PROP_C_W_E)?;
    }
    options.define_property_value_str(ctx, "tools", tools_obj.into(), JS_PROP_C_W_E)?;

    // McpStdio.start(options) — returns a Promise that resolves when stopped.
    let mc = ctx
        .class_constructor(JSMcpStdio::class_id())
        .ok_or_else(|| {
            ctx.throw_internal_error("McpStdio class is not registered");
            -1
        })?;
    let start_fn = mc.get_property_str(ctx, "start")?;
    let p = start_fn.call(ctx, mc.into(), &[options.into()])?;

    // Stop the server on SIGINT / SIGTERM.
    register_signal(ctx, "SIGINT")?;
    register_signal(ctx, "SIGTERM")?;

    // Force process exit once the server has stopped.
    let mut p_promise = Promise::try_from(p)?;
    let exit_fn = ctx.create_native_function(Callback::Func(js_run_exit), "exit", 0, 0)?;
    let noop = ctx.create_native_function(Callback::Func(js_noop), "noop", 0, 0)?;
    let _ = p_promise.then(ctx, exit_fn.into(), noop.into())?;
    Ok(p.into())
}

/// Register `Aike.onSignal(sig, () => McpStdio.stop())`.
fn register_signal(ctx: &mut ScopedContext, sig: &str) -> JSResult {
    let aike = ctx.global_obj().get_property_str(ctx, "Aike")?;
    let onsignal = aike.get_property_str(ctx, "onSignal")?;
    let stop_fn = ctx.create_native_function(Callback::Func(js_mcp_stop), "mcpStop", 0, 0)?;
    let sig_val = js_str(ctx, sig.to_string())?;
    onsignal.call(ctx, aike, &[sig_val, stop_fn.into()])?;
    Ok(JSValue::UNDEFINED)
}

fn js_mcp_stop(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let mc = ctx
        .class_constructor(JSMcpStdio::class_id())
        .ok_or_else(|| {
            ctx.throw_internal_error("McpStdio class is not registered");
            -1
        })?;
    let stop_fn = mc.get_property_str(ctx, "stop")?;
    stop_fn.call(ctx, mc.into(), &[])?;
    Ok(JSValue::UNDEFINED)
}

fn js_run_exit(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let aike = ctx.global_obj().get_property_str(ctx, "Aike")?;
    let exit_fn = aike.get_property_str(ctx, "exit")?;
    exit_fn.call(ctx, aike, &[JSValue::from(0)])?;
    Ok(JSValue::UNDEFINED)
}

fn js_noop(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Ok(JSValue::UNDEFINED)
}

// ── Factory ──

pub(crate) fn js_define_mcp_stdio(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    JSMCPServerDef::new_instance(ctx, JSValue::UNDEFINED, MCPServerDefPayload::new(name))
        .map(JSValue::from)
}

// ── Class definition ──

const MCP_SERVER_DEF_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("version", 1, js_set_string, SET_VERSION),
    FieldDef::FUNC("description", 1, js_set_string, SET_DESCRIPTION),
    FieldDef::FUNC("shared", 1, js_shared, 0),
    FieldDef::FUNC("args", 1, js_args, 0),
    FieldDef::FUNC("env", 2, js_env, 0),
    FieldDef::FUNC("tool", 2, js_tool, 0),
    FieldDef::FUNC("extraFiles", 0, js_extra_files, 0),
    FieldDef::FUNC("beforeRun", 1, js_before_run, 0),
    FieldDef::FUNC("info", 0, js_info, 0),
    FieldDef::FUNC("run", 0, js_run, 0),
];

def_native_object!(
    "MCPServerDef",
    JSMCPServerDef,
    MCPServerDefPayload,
    MCP_SERVER_DEF_PROTO,
    Some(ClassCtorDef::new(js_constructor, 1, true)),
    on_payload_gc: MCPServerDefPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_mcp_server_def(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSMCPServerDef::register(rt)
}
