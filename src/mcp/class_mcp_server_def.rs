// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    ops::{Deref, DerefMut},
};

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JS_GPN_ENUM_ONLY,
    JS_GPN_STR_OR_INT_MASK, JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSResult, JSValue, NativeObject,
    Object, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, http::JSMcpStdio};

/// Environment entry value: a static string, or a JS function resolving to
/// `string | undefined`.
#[derive(Clone)]
enum EnvVal {
    Text(String),
    Func(Object),
}

/// args entry: a static string array, or a JS function resolving to `string[]`.
#[derive(Clone)]
enum ArgsVal {
    Text(Vec<String>),
    Func(Object),
}

/// Per-tool definition: description, JS handler function, and optional args schema.
#[derive(Clone)]
struct McpToolDef {
    desc: String,
    handler: Object,
    args: Option<Object>,
}

enum McpServerType {
    Stdio {
        tools: HashMap<String, McpToolDef>,
        command: Cow<'static, str>,
        command_args: Option<Vec<String>>,
        args_entry: Option<ArgsVal>,
        before_run: Option<Object>,
        shared: bool,
        extra_files: Vec<String>,
    },
    Http {
        url: String,
        tools_schema: Option<Array>,
    },
}

pub(crate) struct McpServerDefPayload {
    server_name: String,
    version: RefCell<Cow<'static, str>>,
    description: RefCell<Cow<'static, str>>,

    /// Transport-agnostic key/value config: environment variables for stdio,
    /// request headers for http. Set via the `env()` builder method.
    env_or_header: RefCell<HashMap<String, EnvVal>>,

    /// The transport-specific definition. `Stdio` carries the stdio-only
    /// fields (shared/extraFiles/args/beforeRun); `Http` carries its own.
    /// Only accessed through the `McpServerDef` builder methods, never
    /// directly from JS.
    server_type: RefCell<McpServerType>,
}

impl McpServerDefPayload {
    fn new_stdio(server_name: String) -> Self {
        Self {
            server_name,
            version: RefCell::new(Cow::Borrowed("1.0.0")),
            description: RefCell::new(Cow::Borrowed("")),
            env_or_header: RefCell::new(HashMap::new()),
            server_type: RefCell::new(McpServerType::Stdio {
                tools: HashMap::new(),
                command: Cow::Borrowed(""),
                command_args: None,
                args_entry: None,
                before_run: None,
                shared: false,
                extra_files: Vec::new(),
            }),
        }
    }

    /// GC trace — keeps referenced JS objects (handlers, callbacks) alive.
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Ok(envs) = self.env_or_header.try_borrow() {
            for v in envs.values() {
                if let EnvVal::Func(f) = v {
                    f.trace(gcx);
                }
            }
        }
        if let Ok(t) = self.server_type.try_borrow() {
            match &*t {
                McpServerType::Stdio {
                    tools,
                    args_entry,
                    before_run,
                    ..
                } => {
                    for t in tools.values() {
                        t.handler.trace(gcx);
                        if let Some(a) = &t.args {
                            a.trace(gcx);
                        }
                    }
                    if let Some(ArgsVal::Func(f)) = args_entry {
                        f.trace(gcx);
                    }
                    if let Some(f) = before_run {
                        f.trace(gcx);
                    }
                }
                McpServerType::Http { .. } => {}
            }
        }
    }
}

fn get_def<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a McpServerDefPayload, i32> {
    val.as_native_object::<JSMcpServerDef>()
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

    JSMcpServerDef::new_instance(ctx, cc.this(), McpServerDefPayload::new_stdio(name))
        .map(JSValue::from)
}

// ── Chain methods ──

/// Magic consts distinguishing which string field `js_set_string` writes.
const FIELD_VERSION: i16 = 0;
const FIELD_DESCRIPTION: i16 = 1;
/// Magic consts for the `mode` / `name` getters in `js_get_string`.
const FIELD_MODE: i16 = 2;
const FIELD_NAME: i16 = 3;
/// Magic consts for the http-only `url` / `headers` getters in `js_http_get`.
const FIELD_URL: i16 = 4;
const FIELD_HEADERS: i16 = 5;

/// Getter/setter for `version` / `description` — the target field is
/// selected via the function's magic const. With no argument it returns the
/// current value; with an argument it stores it and returns `this`.
fn js_set_string(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    if cc.length() == 0 {
        let s = match cc.magic() as i16 {
            FIELD_VERSION => p.version.borrow().to_string(),
            _ => p.description.borrow().to_string(),
        };
        return js_str(ctx, s);
    }

    let v = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    match cc.magic() as i16 {
        FIELD_VERSION => *p.version.borrow_mut() = Cow::Owned(v),
        _ => *p.description.borrow_mut() = Cow::Owned(v),
    }
    Ok(this)
}

fn js_shared(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    if cc.length() == 0 {
        let shared = match &*p.server_type.borrow() {
            McpServerType::Stdio { shared, .. } => *shared,
            McpServerType::Http { .. } => false,
        };
        return Ok(JSValue::from(shared));
    }

    // stdio-only setter: a no-op in http mode where the match fails.
    if let McpServerType::Stdio { shared, .. } = &mut *p.server_type.borrow_mut() {
        *shared = cc.arg(0).to_bool().unwrap_or(false);
    }
    Ok(this)
}

fn js_args(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    if cc.length() == 0 {
        let entry = match &*p.server_type.borrow() {
            McpServerType::Stdio { args_entry, .. } => (*args_entry).clone(),
            McpServerType::Http { .. } => None,
        };
        return match entry {
            None => Ok(JSValue::UNDEFINED),
            Some(ArgsVal::Text(v)) => {
                let mut arr = Array::create(ctx)?;
                for (i, s) in v.iter().enumerate() {
                    let s_val = js_str(ctx, s.clone())?;
                    arr.define_property_value_u32(ctx, i as u32, s_val, JS_PROP_ENUMERABLE)?;
                }
                Ok(arr.into())
            }
            Some(ArgsVal::Func(f)) => Ok(f.into()),
        };
    }

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
        ArgsVal::Text(out)
    };

    // stdio-only setter: a no-op in http mode where the match fails.
    if let McpServerType::Stdio { args_entry, .. } = &mut *p.server_type.borrow_mut() {
        *args_entry = Some(val);
    }
    Ok(this)
}

fn js_env(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    if cc.length() == 0 {
        let mut obj = ctx.create_base_object()?;
        for (k, v) in p.env_or_header.borrow().iter() {
            let val = match v {
                EnvVal::Text(s) => js_str(ctx, s.clone())?,
                EnvVal::Func(f) => f.clone().into(),
            };
            obj.define_property_value_str(ctx, k, val, JS_PROP_ENUMERABLE)?;
        }
        return Ok(obj.into());
    }

    let vars = cc.arg(0);
    let Some(mut obj) = vars.as_object().copied() else {
        ctx.throw_type_error("env expects an object");
        return Err(-1);
    };

    let keys = obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    let mut settings = HashMap::<String, EnvVal>::new();

    for pk in keys.iter() {
        let v = obj.get_property(ctx, pk.atom)?;
        // A literal `undefined` value is omitted (mirrors resolver → undefined).
        if v.is_undefined() {
            continue;
        }

        let key_v = pk.atom.to_value(ctx)?;
        let name = key_v
            .to_string(ctx)?
            .content()
            .encode_utf8(false)
            .to_string();

        let val = if v.as_object().map_or(false, |o| o.is_callable()) {
            EnvVal::Func(v.into_callable(ctx)?)
        } else {
            let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
            EnvVal::Text(s)
        };
        settings.insert(name, val);
    }

    *p.env_or_header.borrow_mut() = settings;
    Ok(this)
}

fn js_stdio_tool(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;

    if let McpServerType::Stdio { tools, .. } = p.server_type.borrow_mut().deref_mut() {
        let name = cc
            .arg(0)
            .to_string(ctx)?
            .content()
            .encode_utf8(false)
            .to_string();

        let opt = cc
            .arg(1)
            .as_object()
            .ok_or_else(|| {
                ctx.throw_type_error("tool def must be an object");
                -1
            })?
            .clone();

        let desc = opt
            .get_property_str(ctx, "desc")
            .and_then(|v| v.to_string(ctx))
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();
        let handler = opt
            .get_property_str(ctx, "handler")
            .and_then(|v| v.into_callable(ctx))
            .map_err(|_| {
                ctx.throw_type_error(&format!("tool '{}' handler must be a function", name));
                -1
            })?;
        let args = opt
            .get_property_str(ctx, "args")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.as_object().copied());

        tools.insert(
            name,
            McpToolDef {
                desc,
                handler,
                args: args.clone(),
            },
        );
    }

    Ok(this)
}

fn js_extra_files(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;

    if let McpServerType::Stdio { extra_files, .. } = p.server_type.borrow_mut().deref_mut() {
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
        *extra_files = paths;
    }
    Ok(this)
}

fn js_before_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;

    let func = cc.arg(0).into_callable(ctx)?;
    if let McpServerType::Stdio { before_run, .. } = p.server_type.borrow_mut().deref_mut() {
        *before_run = Some(func);
    }
    Ok(this)
}

/// Build the JSON-schema `inputSchema` for a tool's `args` DSL object
fn dsl_to_json_schema(ctx: &mut ScopedContext, mut args_obj: Object) -> JSResult {
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

        properties.define_property_value_str(ctx, &name, prop.into(), JS_PROP_ENUMERABLE)?;
    }

    let mut schema = ctx.create_base_object()?;
    let type_val = js_str(ctx, "object".to_string())?;
    schema.define_property_value_str(ctx, "type", type_val, JS_PROP_ENUMERABLE)?;
    schema.define_property_value_str(ctx, "properties", properties.into(), JS_PROP_ENUMERABLE)?;
    if !required.is_empty() {
        let mut req_arr = Array::create(ctx)?;
        for (i, n) in required.iter().enumerate() {
            let v = js_str(ctx, n.clone())?;
            req_arr.define_property_value_u32(ctx, i as u32, v, JS_PROP_ENUMERABLE)?;
        }
        schema.define_property_value_str(ctx, "required", req_arr.into(), JS_PROP_ENUMERABLE)?;
    }
    Ok(schema.into())
}

/// tools JOSN-schema
fn js_tool_schema(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;

    if cc.length() == 0 {
        let ret: JSValue = match p.server_type.borrow().deref() {
            McpServerType::Stdio { tools, .. } => {
                let mut lst = Vec::<JSValue>::with_capacity(tools.len());

                for (name, t) in tools.iter() {
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

                    lst.push(meta.into());
                }

                Array::from_values(ctx, lst)?.into()
            }
            McpServerType::Http {
                tools_schema: Some(schema),
                ..
            } => (*schema).into(),
            _ => JSValue::UNDEFINED,
        };

        Ok(ret)
    } else {
        match p.server_type.borrow_mut().deref_mut() {
            McpServerType::Http { tools_schema, .. } => {
                if let Ok(schema) = Array::try_from(cc.arg(0)) {
                    // TODO: validate schema
                    tools_schema.replace(schema);
                } else {
                    eprintln!("Error: tool schema must be an object");
                }
            }
            _ => {
                eprintln!("Warn: set tools schema for stdio is ignored");
            }
        }
        Ok(this)
    }
}

/// Getter for `mode` / `name` — the target field is selected via the
/// function's magic const (`FIELD_MODE` / `FIELD_NAME`).
fn js_get_string(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;
    match cc.magic() as i16 {
        FIELD_MODE => {
            let mode = match &*p.server_type.borrow() {
                McpServerType::Stdio { .. } => "stdio",
                McpServerType::Http { .. } => "http",
            };
            Ok(ctx.create_string_from_static(mode)?.into())
        }
        FIELD_NAME => Ok(js_str(ctx, p.server_name.clone())?),
        _ => Ok(JSValue::UNDEFINED),
    }
}

/// Getter for the http-only views `url` / `headers`. Returns the http server's
/// endpoint url and registered request headers; for a stdio server these return
/// `undefined` (that transport has no url / headers). The target field is selected
/// via the function's magic const (`FIELD_URL` / `FIELD_HEADERS`).
fn js_http_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_def(ctx, &this)?;

    match p.server_type.borrow().deref() {
        McpServerType::Http { url, .. } => match cc.magic() as i16 {
            FIELD_URL => js_str(ctx, url.clone()),
            FIELD_HEADERS => {
                let mut obj = ctx.create_base_object()?;
                for (k, v) in p.env_or_header.borrow().iter() {
                    let val = match v {
                        EnvVal::Text(s) => js_str(ctx, s.clone())?,
                        EnvVal::Func(f) => f.clone().into(),
                    };
                    obj.define_property_value_str(ctx, k, val, JS_PROP_ENUMERABLE)?;
                }
                Ok(obj.into())
            }
            _ => Ok(JSValue::UNDEFINED),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

fn js_launch_server(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let before_run = match &*get_def(ctx, &this)?.server_type.borrow() {
        McpServerType::Stdio { before_run, .. } => before_run.clone(),
        McpServerType::Http { .. } => None,
    };

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

/// Start the stdio MCP server: build the options object, call `McpStdio.start`,
/// then register signal handlers that stop it and force-exit once it stops.
/// The http transport is a remote connection and never calls `run()`; reaching
/// it here is a bug, so the http branch is `unreachable!()`.
fn run_start(ctx: &mut ScopedContext, this: JSValue) -> JSResult {
    let p = get_def(ctx, &this)?;
    let tools: Vec<(String, McpToolDef)> = match &*p.server_type.borrow() {
        McpServerType::Stdio { tools, .. } => {
            tools.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        }
        McpServerType::Http { .. } => unreachable!("http McpServerDef never runs"),
    };

    let server_name = p.server_name.clone();
    let version = p.version.borrow().clone();
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
    JSMcpServerDef::new_instance(
        ctx,
        JSValue::UNDEFINED,
        McpServerDefPayload::new_stdio(name),
    )
    .map(JSValue::from)
}

// ── Class definition ──

const MCP_SERVER_DEF_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("name", Some(js_get_string), None, FIELD_NAME),
    FieldDef::GETSET("mode", Some(js_get_string), None, FIELD_MODE),
    FieldDef::GETSET("toolSchema", Some(js_tool_schema), Some(js_tool_schema), 0),
    FieldDef::GETSET("url", Some(js_http_get), None, FIELD_URL),
    FieldDef::GETSET("headers", Some(js_http_get), None, FIELD_HEADERS),
    FieldDef::FUNC("version", 1, js_set_string, FIELD_VERSION),
    FieldDef::FUNC("description", 1, js_set_string, FIELD_DESCRIPTION),
    FieldDef::FUNC("shared", 1, js_shared, 0),
    FieldDef::FUNC("args", 1, js_args, 0),
    FieldDef::FUNC("env", 1, js_env, 0),
    FieldDef::FUNC("tool", 2, js_stdio_tool, 0),
    FieldDef::FUNC("extraFiles", 0, js_extra_files, 0),
    FieldDef::FUNC("beforeRun", 1, js_before_run, 0),
    FieldDef::FUNC("run", 0, js_launch_server, 0),
];

def_native_object!(
    "McpServerDef",
    JSMcpServerDef,
    McpServerDefPayload,
    MCP_SERVER_DEF_PROTO,
    Some(ClassCtorDef::new(js_constructor, 1, true)),
    on_payload_gc: McpServerDefPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_mcp_server_def(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSMcpServerDef::register(rt)
}
