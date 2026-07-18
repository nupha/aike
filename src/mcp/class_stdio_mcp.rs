// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam::channel;
use linkme::distributed_slice;
use rmcp::service::RoleServer;
use zuqe::{
    Array, CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JS_GPN_ENUM_ONLY,
    JS_GPN_STR_OR_INT_MASK, JS_PROP_ENUMERABLE, JSResult, JSValue, NativeObject, Object,
    PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, http::IncomingRequest};

use super::{
    CommandArgs, EnvVal,
    class_mcp::{JSBaseMcp, get_payload, js_str},
    service::{
        McpService, McpServicePayload, McpTransport, handle_mcp_tool_call, parse_mcp_define,
    },
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

/// The `StdioMcp` / `HttpMcp` payload type (`McpServicePayload`) and its
/// `McpTransport` enum now live in `service.rs` (merged from the old
/// `StdioMcpPayload` + `McpEndpoint`, see mcp-service.md §3).

/// Constructor callback — `StdioMcp` is NOT constructable. Instances must be
/// created via the static `StdioMcp.define(name, tools, options)`. Calling it
/// with `new` (or as a plain function) throws a TypeError.
fn js_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error("StdioMcp cannot be constructed with `new`; use StdioMcp.define(config)");
    Err(-1)
}

/// StdioMcp.define(config) — static factory taking a single config object.
fn js_stdio_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let Some(cfg) = cc.arg(0).as_object().copied() else {
        ctx.throw_type_error("StdioMcp.define(config) — config must be an object");
        return Err(-1);
    };

    // command — allow undefined/string.
    let mut command = String::new();
    if let Some(v) = cfg
        .get_property_str(ctx, "command")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        command = v.to_string(ctx)?.content().encode_utf8(false).to_string();
    }

    // env — Record<string, string | (() => string | undefined)>.
    let mut command_env = HashMap::new();
    if let Some(v) = cfg
        .get_property_str(ctx, "env")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        if let Some(o) = v.as_object().copied() {
            command_env = parse_env_object(ctx, o)?;
        }
    }

    // args — string[] | (() => string[]).
    let mut command_args = CommandArgs::Empty;
    if let Some(v) = cfg
        .get_property_str(ctx, "args")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        command_args = if v.as_object().map_or(false, |o| o.is_callable()) {
            CommandArgs::Func(v.into_callable(ctx)?)
        } else {
            let arr = Array::try_from(v)?;
            let len = arr.get_length_prop(ctx)? as usize;
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
            CommandArgs::Text(out)
        };
    }

    // beforeRun — () => Promise<void>.
    let mut before_run = None;
    if let Some(v) = cfg
        .get_property_str(ctx, "beforeRun")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        before_run = Some(v.into_callable(ctx)?);
    }

    // extraFiles — string[].
    let mut extra_files = Vec::new();
    if let Some(v) = cfg
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
            extra_files = paths;
        }
    }

    let transport = McpTransport::Stdio {
        command,
        command_env,
        command_args,
        before_run,
        extra_files,
    };

    // Common config (name/version/instructions/tools) + compiled schemas.
    let payload = parse_mcp_define(ctx, cfg, transport)?;

    JSStdioMcp::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

const FIELD_EXTRA_FILES: i16 = 8;
const FIELD_BEFORE_RUN: i16 = 9;
const FIELD_SPAWN_ENV: i16 = 10;
const FIELD_SPAWN_ARGS: i16 = 11;
const FIELD_COMMAND: i16 = 12;

fn js_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    match cc.magic() as i16 {
        FIELD_EXTRA_FILES => match &*p.transport.borrow() {
            McpTransport::Stdio { extra_files, .. } => {
                let files: Vec<String> = extra_files.clone();
                let mut lst = Vec::with_capacity(files.len());
                for f in &files {
                    lst.push(js_str(ctx, f.clone())?.into());
                }
                Ok(Array::from_values(ctx, lst)?.into())
            }
            _ => Ok(Array::with_capacity(ctx, 0)?.into()),
        },
        FIELD_BEFORE_RUN => match &*p.transport.borrow() {
            McpTransport::Stdio { before_run, .. } => match before_run {
                None => Ok(JSValue::NULL),
                Some(f) => Ok(f.clone().into()),
            },
            _ => Ok(JSValue::NULL),
        },
        FIELD_COMMAND => match &*p.transport.borrow() {
            McpTransport::Stdio { command, .. } => js_str(ctx, command.clone()),
            _ => Ok(JSValue::UNDEFINED),
        },

        _ => Ok(JSValue::UNDEFINED),
    }
}

/// Instance `run()` — launch this server's stdio transport via the shared
/// `start_stdio_server`, which owns the process-wide running state. Mirrors
/// `McpServerDef.run()`: stops on SIGINT/SIGTERM and force-exits once the
/// server stops.
fn js_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    // beforeRun — user init hook, run once before serving (memo/mcp-service.md §2.2).
    // On the JS thread here; fire-and-forget (async hook may still be pending when
    // serve starts — awaiting a JS Promise would block this thread).
    if let McpTransport::Stdio { before_run, .. } = &*p.transport.borrow() {
        if let Some(hook) = before_run.clone() {
            if let Err(e) = hook.call(ctx, this, &[]) {
                return Err(e);
            }
        }
    }

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

/// Resolve `spawnEnv` / `spawnArgs` against the agent's context.
fn js_spawn_resolve(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    // ns arg — the agent's McpCtx (Record<string,string>); default to {}.
    let nsobj = match cc.arg(0).as_object().copied() {
        Some(o) => o,
        None => ctx.create_base_object()?,
    };

    match &*p.transport.borrow() {
        McpTransport::Stdio {
            command_env: spawn_env,
            command_args: spawn_args,
            ..
        } => match cc.magic() as i16 {
            FIELD_SPAWN_ENV => {
                let mut env = ctx.create_base_object()?;
                for (k, v) in spawn_env.iter() {
                    let val = match v {
                        EnvVal::Text(s) => js_str(ctx, s.clone())?.into(),
                        EnvVal::Func(f) => {
                            let r = f.call(ctx, JSValue::UNDEFINED, &[nsobj.into()])?;
                            if r.is_undefined() {
                                continue;
                            }
                            r
                        }
                    };
                    env.define_property_value_str(ctx, k, val, JS_PROP_ENUMERABLE)?;
                }
                Ok(env.into())
            }
            FIELD_SPAWN_ARGS => match spawn_args {
                CommandArgs::Empty => Ok(Array::with_capacity(ctx, 0)?.into()),
                CommandArgs::Text(v) => {
                    let mut lst = Vec::with_capacity(v.len());
                    for s in v {
                        lst.push(js_str(ctx, s.clone())?.into());
                    }
                    Ok(Array::from_values(ctx, lst)?.into())
                }
                CommandArgs::Func(f) => f.call(ctx, JSValue::UNDEFINED, &[nsobj.into()]),
            },
            _ => Ok(JSValue::UNDEFINED),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

fn start_stdio_server(ctx: &mut ScopedContext, p: &McpServicePayload) -> Result<Promise, i32> {
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

/// Parse an env object (`Record<string, string | (() => string | undefined)>`)
/// into the internal `EnvVal` map. Used by the `spawnEnv` option.
fn parse_env_object(
    ctx: &mut ScopedContext,
    mut obj: Object,
) -> Result<HashMap<String, EnvVal>, i32> {
    let keys = obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    let mut settings = HashMap::<String, EnvVal>::new();

    for pk in keys.iter() {
        let v = obj.get_property(ctx, pk.atom)?;
        // A literal `undefined` value is omitted (mirrors resolver → undefined).
        if v.is_undefined() {
            continue;
        }

        let name = pk
            .atom
            .to_value(ctx)?
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

    Ok(settings)
}

/// Register `Aike.onSignal(sig, () => StdioMcp.stop())` so the server stops
/// gracefully on SIGINT / SIGTERM.
fn register_signal(ctx: &mut ScopedContext, sig: &str) -> JSResult {
    let aike = ctx.global_obj().get_property_str(ctx, "Aike")?;
    let onsignal = aike.get_property_str(ctx, "onSignal")?;
    let stop_fn = ctx.create_native_function(Callback::Func(js_stop), "mcpStop", 0, 0)?;
    let sig_val = js_str(ctx, sig.to_string())?;
    onsignal.call(ctx, aike, &[sig_val, stop_fn.into()])?;
    Ok(JSValue::UNDEFINED)
}

// Static methods defined on the class constructor (not prototype)
const STDIO_MCP_STATIC_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("define", 1, js_stdio_define, 0),
    FieldDef::FUNC("stop", 0, js_stop, 0),
    FieldDef::FUNC("isRunning", 0, js_is_running, 0),
];

// Constructor + static entries, built once as a const so the macro's
// `$ctor:expr` matcher sees a plain identifier rather than a method-call
// chain (which can confuse macro argument parsing).
const STDIO_MCP_CTOR: ClassCtorDef =
    ClassCtorDef::new(js_constructor, 1, true).entries(STDIO_MCP_STATIC_ENTRIES);

// Instance prototype — the stdio-only surface (shared getters/methods are
// inherited from `BaseMcp`, see class_mcp.rs).
const STDIO_MCP_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("command", Some(js_get), None, FIELD_COMMAND),
    FieldDef::GETSET("extraFiles", Some(js_get), None, FIELD_EXTRA_FILES),
    FieldDef::GETSET("beforeRun", Some(js_get), None, FIELD_BEFORE_RUN),
    FieldDef::FUNC("spawnEnv", 1, js_spawn_resolve, FIELD_SPAWN_ENV),
    FieldDef::FUNC("spawnArgs", 1, js_spawn_resolve, FIELD_SPAWN_ARGS),
    FieldDef::FUNC("run", 0, js_run, 0),
];

def_native_object!(
    "StdioMcp",
    JSStdioMcp,
    McpServicePayload,
    STDIO_MCP_PROTO,
    Some(STDIO_MCP_CTOR),
    extends: JSBaseMcp::class_id,
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_stdio_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSStdioMcp::register(rt)
}
