// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam::channel;
use linkme::distributed_slice;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::service::RoleServer;
use zuqe::{
    Array, CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JS_GPN_ENUM_ONLY,
    JS_GPN_STR_OR_INT_MASK, JSResult, JSValue, Object, PollSender, Runtime,
    ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    http::{IncomingRequest, build_input_schema_from_args},
};

use super::{
    ArgsVal, EnvVal,
    class_mcp_server_def::{parse_env_object, register_signal},
    service::{McpSchemas, McpService, handle_mcp_tool_call},
};

struct StdioMcpShutdown {
    /// Handle to the running rmcp service. Taken by waiting() on shutdown.
    running_service: Mutex<Option<rmcp::service::RunningService<RoleServer, McpService>>>,
    /// Cancellation token for sync stop signal.
    cancellation_token: Mutex<Option<rmcp::service::RunningServiceCancellationToken>>,
    /// True while the server is actively serving.
    is_running: AtomicBool,
}

/// Shutdown control state in static — only Send+Sync types.
/// OnceLock is set once by start() and never released.
static MCP_STDIO_STATE: OnceLock<StdioMcpShutdown> = OnceLock::new();

// Thread-local GC roots for JS handler objects (Object is !Send, so can't
// live in a static). Set in start(), cleared on shutdown.
thread_local! {
    static STDIO_JS_HANDLERS: RefCell<Vec<Object>> = RefCell::new(Vec::new());
}

/// GC trace callback for the stdio MCP server's JS handler objects.
pub(crate) fn gc_trace_stdio_state(gcx: &mut GcTraceCtx) {
    STDIO_JS_HANDLERS.with(|handlers| {
        for h in handlers.borrow().iter() {
            h.trace(gcx);
        }
    });
}

/// Builder payload for an instantiable `StdioMcp` server. Holds the stdio-only
/// options (env / args / beforeRun / extraFiles / shared flag) plus the
/// compiled tool registry (`schemas`) and the JS handler closures. The raw
/// tool DSL is consumed at `define()` time, so only the compiled form remains.
pub(crate) struct StdioMcpPayload {
    server_name: String,
    version: RefCell<Cow<'static, str>>,
    instructions: RefCell<Cow<'static, str>>,

    env: RefCell<HashMap<String, EnvVal>>,
    args_entry: RefCell<Option<ArgsVal>>,
    before_run: RefCell<Option<Object>>,
    /// Whether this server is a shared (reusable) stdio server.
    is_shared: Cell<bool>,
    extra_files: RefCell<Vec<String>>,

    /// Compiled RMCP tool registry, shared with the tokio service.
    schemas: Arc<RwLock<McpSchemas>>,
    /// JS handler objects — GC roots + dispatch targets for the poller.
    js_handlers: RefCell<Vec<Object>>,
}

impl StdioMcpPayload {
    fn new(server_name: String) -> Self {
        Self {
            server_name,
            version: RefCell::new(Cow::Borrowed("1.0.0")),
            instructions: RefCell::new(Cow::Borrowed("")),
            env: RefCell::new(HashMap::new()),
            args_entry: RefCell::new(None),
            before_run: RefCell::new(None),
            is_shared: Cell::new(false),
            extra_files: RefCell::new(Vec::new()),
            schemas: Arc::new(RwLock::new(McpSchemas {
                tools: Vec::new(),
                name_to_index: HashMap::new(),
                native_map: HashMap::new(),
            })),
            js_handlers: RefCell::new(Vec::new()),
        }
    }

    /// GC trace — keeps referenced JS objects (handlers, callbacks) alive.
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for v in self.env.borrow().values() {
            if let EnvVal::Func(f) = v {
                f.trace(gcx);
            }
        }
        if let Some(ArgsVal::Func(f)) = &*self.args_entry.borrow() {
            f.trace(gcx);
        }
        if let Some(f) = &*self.before_run.borrow() {
            f.trace(gcx);
        }
        for h in self.js_handlers.borrow().iter() {
            h.trace(gcx);
        }
    }

    /// Build the MCP `initialize` handshake result from this payload. Mirrors
    /// the `ServerInfo` (== `InitializeResult`) the live server advertises, so
    /// it stays in sync with the handshake.
    fn make_server_info(&self) -> ServerInfo {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        let info = ServerInfo::new(capabilities)
            .with_server_info(Implementation::new(
                self.server_name.clone(),
                self.version.borrow().to_string(),
            ))
            .with_protocol_version(ProtocolVersion::V_2025_06_18);
        if self.instructions.borrow().is_empty() {
            info
        } else {
            info.with_instructions(self.instructions.borrow().to_string())
        }
    }

    /// Serialize the registered tools into a JS `Array` of JSON-schema objects.
    /// Each `Tool` is `#[derive(Serialize)]`, so it is converted via serde_json
    /// rather than hand-built.
    fn make_schemas_json(&self, ctx: &mut ScopedContext) -> Result<Array, i32> {
        let guard = self.schemas.read().map_err(|e| {
            ctx.throw_internal_error(&format!("schema lock poisoned: {}", e));
            -1
        })?;

        let mut lst = Vec::with_capacity(guard.tools.len());
        for tool in guard.tools.iter() {
            let val = serde_json::to_value(tool).map_err(|e| {
                ctx.throw_internal_error(&format!("tool serialize failed: {}", e));
                -1
            })?;
            let meta = JSValue::from_serde_json(ctx, &val).unwrap_or_else(|_| {
                ctx.create_base_object()
                    .map(|o| o.into())
                    .unwrap_or(JSValue::UNDEFINED)
            });
            lst.push(meta);
        }

        Array::from_values(ctx, lst)
    }
}

/// Create a JS string value, taking ownership of `s` so no extra allocation
/// is needed when the caller already holds a `String`.
fn js_str(ctx: &mut ScopedContext, s: String) -> JSResult {
    Ok(ctx.create_string_from_utf8(s.into_bytes())?.into())
}

fn get_payload<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a StdioMcpPayload, i32> {
    val.as_native_object::<JSStdioMcp>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not a StdioMcp instance");
            -1
        })
}

/// Constructor callback — `StdioMcp` is NOT constructable. Instances must be
/// created via the static `StdioMcp.define(name, tools, options)`. Calling it
/// with `new` (or as a plain function) throws a TypeError.
fn js_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error(
        "StdioMcp cannot be constructed with `new`; use StdioMcp.define(name, tools, options)",
    );
    Err(-1)
}

/// StdioMcp.define(name, tools, options) — static factory.
///
/// `tools` is the required second argument: an object-keyed DSL
/// `{ name: { desc, handler, args } }`. The optional third `options` object
/// carries version / instructions / env / args / beforeRun / shared /
/// extraFiles (everything except `tools`, which is lifted to its own arg).
fn js_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    // tools — required second argument.
    let tools_val = cc.arg(1);
    let Some(tools_obj) = tools_val.as_object().copied() else {
        ctx.throw_type_error("`tools` must be an object");
        return Err(-1);
    };

    let mut payload = StdioMcpPayload::new(name);

    // Optional third `options` object (everything except tools).
    let opt = cc.arg(2);
    if !opt.is_undefined() {
        let Some(obj) = opt.as_object().copied() else {
            ctx.throw_type_error("StdioMcp.define options must be an object");
            return Err(-1);
        };
        apply_stdio_options(ctx, &mut payload, obj)?;
    }

    // tools is lifted out of options: parse it directly from the 2nd arg.
    parse_tools_defination(ctx, &mut payload, tools_obj)?;

    JSStdioMcp::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

const FIELD_VERSION: i16 = 0;
const FIELD_INSTRUCTIONS: i16 = 1;
const FIELD_NAME: i16 = 3;
const FIELD_SHARED: i16 = 6;
const FIELD_EXTRA_FILES: i16 = 8;
const FIELD_BEFORE_RUN: i16 = 9;

fn js_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    match cc.magic() as i16 {
        FIELD_NAME => js_str(ctx, p.server_name.clone()),
        FIELD_VERSION => js_str(ctx, p.version.borrow().to_string()),
        FIELD_INSTRUCTIONS => js_str(ctx, p.instructions.borrow().to_string()),
        FIELD_SHARED => Ok(JSValue::from(p.is_shared.get())),
        FIELD_EXTRA_FILES => {
            let files: Vec<String> = p.extra_files.borrow().clone();
            let mut lst = Vec::with_capacity(files.len());
            for f in &files {
                lst.push(js_str(ctx, f.clone())?.into());
            }
            Ok(Array::from_values(ctx, lst)?.into())
        }
        FIELD_BEFORE_RUN => match &*p.before_run.borrow() {
            None => Ok(JSValue::NULL),
            Some(f) => Ok(f.clone().into()),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

/// tools JSON-schema getter (mirrors `McpServerDef.toolSchema`). Setting it on
/// a stdio server is a no-op, so the setter just returns `this`.
fn js_tools_list(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    if cc.length() == 0 {
        p.make_schemas_json(ctx).map(JSValue::from)
    } else {
        Ok(this)
    }
}

/// `initializeInfo` getter — returns the object advertised in the MCP
/// `initialize` handshake result. Built from `make_server_info` (== the
/// `InitializeResult` the server actually returns), then serialized to a JS
/// object via serde_json.
fn js_initialize_info(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    let info = p.make_server_info();
    let val = serde_json::to_value(info).map_err(|e| {
        ctx.throw_internal_error(&format!("server info serialize failed: {}", e));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

/// Instance `run()` — launch this server's stdio transport via the shared
/// `start_stdio_server`, which owns the process-wide running state. Mirrors
/// `McpServerDef.run()`: stops on SIGINT/SIGTERM and force-exits once the
/// server stops.
fn js_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    // Launch the stdio server
    let mut promise = start_stdio_server(ctx, p)?;

    // Stop on SIGINT / SIGTERM.
    register_signal(ctx, "SIGINT")?;
    register_signal(ctx, "SIGTERM")?;

    // Force process exit once the server has stopped.
    let exit_fn = ctx.create_native_function(Callback::Func(js_run_exit), "exit", 0, 0)?;
    let noop = ctx.create_native_function(Callback::Func(js_noop), "noop", 0, 0)?;
    let _ = promise.then(ctx, exit_fn.into(), noop.into())?;

    Ok(promise.into())
}

/// Apply the `StdioMcp.define(name, tools, options)` options object onto a
/// fresh payload. `tools` is handled separately (it is the lifted 2nd arg),
/// so this covers version / instructions / env / args / beforeRun / shared /
/// extraFiles.
fn apply_stdio_options(
    ctx: &mut ScopedContext,
    p: &mut StdioMcpPayload,
    opt: Object,
) -> Result<(), i32> {
    // version
    if let Some(v) = opt
        .get_property_str(ctx, "version")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *p.version.borrow_mut() = Cow::Owned(s);
    }

    // instructions
    if let Some(v) = opt
        .get_property_str(ctx, "instructions")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *p.instructions.borrow_mut() = Cow::Owned(s);
    }

    // env — Record<string, string | (() => string | undefined)>.
    if let Some(v) = opt
        .get_property_str(ctx, "env")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        if let Some(obj) = v.as_object().copied() {
            *p.env.borrow_mut() = parse_env_object(ctx, obj)?;
        }
    }

    // args — string[] | (() => string[]).
    if let Some(v) = opt
        .get_property_str(ctx, "args")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let val = if v.as_object().map_or(false, |o| o.is_callable()) {
            ArgsVal::Func(v.into_callable(ctx)?)
        } else {
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
        *p.args_entry.borrow_mut() = Some(val);
    }

    // beforeRun — () => Promise<void>.
    if let Some(v) = opt
        .get_property_str(ctx, "beforeRun")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let func = v.into_callable(ctx)?;
        *p.before_run.borrow_mut() = Some(func);
    }

    // shared — boolean.
    if let Some(v) = opt
        .get_property_str(ctx, "shared")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let b = v.to_bool().unwrap_or(false);
        p.is_shared.set(b);
    }

    // extraFiles — string[].
    if let Some(v) = opt
        .get_property_str(ctx, "extraFiles")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        if let Some(arr) = v.as_object().copied() {
            let len = arr
                .get_property_str(ctx, "length")
                .and_then(|l| l.to_number(ctx))
                .map(|n| n.to_i32())? as usize;
            let mut paths = Vec::with_capacity(len);
            for i in 0..len {
                let s = arr
                    .get_property_u32(ctx, i as u32)?
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                paths.push(s);
            }
            *p.extra_files.borrow_mut() = paths;
        }
    }

    Ok(())
}

/// Parse the object-keyed tool DSL `{ name: { desc, handler, args } }` from a
/// JS object and compile it directly into the payload's `schemas` registry and
/// `js_handlers` closures. The raw DSL is consumed here — only the compiled
/// form is retained for launch.
fn parse_tools_defination(
    ctx: &mut ScopedContext,
    p: &mut StdioMcpPayload,
    mut tools_obj: Object,
) -> Result<(), i32> {
    let pks = tools_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    let mut tools: Vec<rmcp::model::Tool> = Vec::with_capacity(pks.len());
    let mut name_to_index: HashMap<String, usize> = HashMap::new();
    let mut js_handlers: Vec<Object> = Vec::with_capacity(pks.len());

    for pk in pks.iter() {
        let name = pk
            .atom
            .to_value(ctx)
            .and_then(|v| v.to_string(ctx))
            .map(|s| s.content().encode_utf8(false).to_string())
            .map_err(|_| {
                ctx.throw_type_error("tool name must be a string");
                -1
            })?;
        if name.is_empty() {
            continue;
        }

        let val = tools_obj.get_property(ctx, pk.atom).map_err(|_| {
            ctx.throw_type_error(&format!("tool '{}' must be an object", name));
            -1
        })?;
        let tool_obj = val
            .as_object()
            .ok_or_else(|| {
                ctx.throw_type_error(&format!("tool '{}' must be an object", name));
                -1
            })?
            .clone();

        let desc = tool_obj
            .get_property_str(ctx, "desc")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();
        let handler = tool_obj
            .get_property_str(ctx, "handler")
            .and_then(|v| v.into_callable(ctx))
            .map_err(|_| {
                ctx.throw_type_error(&format!("tool '{}' handler must be a function", name));
                -1
            })?;
        let args = tool_obj
            .get_property_str(ctx, "args")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.as_object().copied());

        let input_schema_json = match args {
            Some(args_obj) => build_input_schema_from_args(ctx, args_obj.clone())?
                .unwrap_or(serde_json::json!({"type": "object"})),
            None => serde_json::json!({"type": "object"}),
        };
        let input_schema_obj: serde_json::Map<String, serde_json::Value> = match input_schema_json {
            serde_json::Value::Object(m) => m,
            _ => serde_json::json!({"type": "object"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let tool = rmcp::model::Tool::new(
            name.clone(),
            desc,
            std::sync::Arc::new(input_schema_obj.into()),
        );

        let idx = tools.len();
        tools.push(tool);
        name_to_index.insert(name.clone(), idx);
        js_handlers.push(handler);
    }

    p.schemas = Arc::new(RwLock::new(McpSchemas {
        tools,
        name_to_index,
        native_map: HashMap::new(),
    }));
    *p.js_handlers.borrow_mut() = js_handlers;

    Ok(())
}

fn start_stdio_server(ctx: &mut ScopedContext, p: &StdioMcpPayload) -> Result<Promise, i32> {
    if MCP_STDIO_STATE.get().is_some() {
        ctx.throw_internal_error(
            "Has running stdio mcp server — it can only be started once per process.",
        );
        return Err(-1);
    }

    // Route all console.log output to stderr to keep stdout clean for MCP protocol.
    // Refuse if stdio is already captured by another (concurrent) server.
    if let Err(msg) = crate::repl::caupture_stdio(true) {
        ctx.throw_type_error(msg.as_str());
        return Err(-1);
    }

    let server_name = p.server_name.clone();
    let server_version = p.version.borrow().to_string();
    let server_instructions = p.instructions.borrow().to_string();

    // Read compiled schemas + JS handlers produced at define() time.
    let schemas = p.schemas.clone();
    let js_handlers = p.js_handlers.borrow().clone();

    // Create crossbeam channel (same IncomingRequest mechanism as HTTP mode)
    let (js_tx, js_rx) = channel::unbounded::<IncomingRequest>();

    // Build McpService directly (stdio: no native handlers, single endpoint).
    let service = McpService::new(
        schemas,
        Vec::new(),
        js_tx,
        0,
        server_name,
        server_version,
        server_instructions,
    );

    // Start JS poller job for tool call dispatch
    let mcp_tables = RefCell::new(vec![js_handlers.clone()]);
    let is_running = std::sync::Arc::new(AtomicBool::new(true));

    crate::helpers::start_channel_poller(
        ctx,
        "stdio_mcp",
        js_rx,
        move |incoming: IncomingRequest, cx| {
            match incoming {
                IncomingRequest::McpToolCall(req) => {
                    handle_mcp_tool_call(cx, req, &mcp_tables.borrow());
                }
                IncomingRequest::Http(_)
                | IncomingRequest::WebSocketOpen { .. }
                | IncomingRequest::WebSocketData { .. } => {
                    // should never happen in stdio mode
                }
            }

            true
        },
        {
            let b = is_running.clone();
            move || b.load(Ordering::Acquire)
        },
        || {},
    );

    // Create Promise that resolves when server stops
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_, _| Ok(JSValue::UNDEFINED))?;

    // Store js_handlers in thread_local for GC tracing (Object is !Send)
    STDIO_JS_HANDLERS.with(|h| {
        *h.borrow_mut() = js_handlers;
    });

    // Initialize shutdown state (only Send+Sync fields in static)
    MCP_STDIO_STATE
        .set(StdioMcpShutdown {
            running_service: Mutex::new(None),
            cancellation_token: Mutex::new(None),
            is_running: AtomicBool::new(true),
        })
        .ok();

    // Spawn background tokio task to run rmcp serve_server
    tokio::spawn(async move {
        let transport = (tokio::io::stdin(), tokio::io::stdout());

        // Step 1: Initialize (handshake) — returns quickly on error or cancel
        let running = match rmcp::serve_server(service, transport).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[StdioMcp] init error: {:?}", e);
                if let Some(state) = MCP_STDIO_STATE.get() {
                    state.is_running.store(false, Ordering::Release);
                    // Also stop the poller
                    // (is_running_poller is the poller's keep-alive flag)
                }
                is_running.store(false, Ordering::Release);
                let _ = crate::repl::caupture_stdio(false);
                let _ = PollSender::from(tx).send_async(Ok(())).await;
                return;
            }
        };

        // Step 2: Store RunningService + cancellation token
        let ct = running.cancellation_token();
        if let Some(state) = MCP_STDIO_STATE.get() {
            *state.cancellation_token.lock().unwrap() = Some(ct);
            *state.running_service.lock().unwrap() = Some(running);
        }

        // Step 3: Wait for termination (natural EOF, cancel, or error)
        // Take the RunningService out and call waiting() which consumes it.
        let running = MCP_STDIO_STATE
            .get()
            .and_then(|s| s.running_service.lock().unwrap().take());
        if let Some(r) = running {
            let _ = r.waiting().await;
        }

        // Step 4: Cleanup — mark stopped and resolve Promise
        if let Some(state) = MCP_STDIO_STATE.get() {
            state.is_running.store(false, Ordering::Release);
        }
        is_running.store(false, Ordering::Release);
        let _ = crate::repl::caupture_stdio(false);
        let _ = PollSender::from(tx).send_async(Ok(())).await;
    });

    Ok(promise)
}

fn js_stop(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let Some(state) = MCP_STDIO_STATE.get() else {
        return Ok(JSValue::UNDEFINED); // never started
    };
    if !state.is_running.load(Ordering::Acquire) {
        return Ok(JSValue::UNDEFINED); // already stopped
    }

    // Take the cancellation token and fire it (sync, no blocking)
    let token = state.cancellation_token.lock().unwrap().take();
    if let Some(t) = token {
        t.cancel();
    }

    // The background task's waiting() will return, triggering cleanup
    // and Promise resolution.

    Ok(JSValue::UNDEFINED)
}

/// StdioMcp.isRunning() — returns true if the server is currently serving.
fn js_is_running(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    match MCP_STDIO_STATE.get() {
        Some(state) => Ok(JSValue::from(state.is_running.load(Ordering::Acquire))),
        None => Ok(JSValue::from(false)),
    }
}

/// Force process exit once the server stops (used by `run`'s `.then`).
fn js_run_exit(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let aike = ctx.global_obj().get_property_str(ctx, "Aike")?;
    let exit_fn = aike.get_property_str(ctx, "exit")?;
    exit_fn.call(ctx, aike, &[JSValue::from(0)])?;
    Ok(JSValue::UNDEFINED)
}

fn js_noop(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Ok(JSValue::UNDEFINED)
}

// Static methods defined on the class constructor (not prototype)
const STDIO_MCP_STATIC_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("define", 3, js_define, 0),
    FieldDef::FUNC("stop", 0, js_stop, 0),
    FieldDef::FUNC("isRunning", 0, js_is_running, 0),
];

// Constructor + static entries, built once as a const so the macro's
// `$ctor:expr` matcher sees a plain identifier rather than a method-call
// chain (which can confuse macro argument parsing).
const STDIO_MCP_CTOR: ClassCtorDef =
    ClassCtorDef::new(js_constructor, 1, true).entries(STDIO_MCP_STATIC_ENTRIES);

// Instance prototype — the stdio builder surface, mirroring McpServerDef.
const STDIO_MCP_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("initializeInfo", Some(js_initialize_info), None, 0),
    FieldDef::GETSET("toolsList", Some(js_tools_list), Some(js_tools_list), 0),
    FieldDef::GETSET("shared", Some(js_get), None, FIELD_SHARED),
    FieldDef::GETSET("extraFiles", Some(js_get), None, FIELD_EXTRA_FILES),
    FieldDef::GETSET("beforeRun", Some(js_get), None, FIELD_BEFORE_RUN),
    FieldDef::FUNC("run", 0, js_run, 0),
    //
    // not required
    //
    FieldDef::GETSET("name", Some(js_get), None, FIELD_NAME),
    FieldDef::GETSET("version", Some(js_get), None, FIELD_VERSION),
    FieldDef::GETSET("instructions", Some(js_get), None, FIELD_INSTRUCTIONS),
];

def_native_object!(
    "StdioMcp",
    JSStdioMcp,
    StdioMcpPayload,
    STDIO_MCP_PROTO,
    Some(STDIO_MCP_CTOR),
    on_payload_gc: StdioMcpPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_mcp_stdio(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSStdioMcp::register(rt)
}
