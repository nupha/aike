// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    collections::HashMap,
    process::ExitCode,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use rmcp::service::{RoleServer, serve_server_with_ct};
use tokio_util::sync::CancellationToken;
use zuqe::{
    Array, CallContext, Callback, ClassCtorDef, FieldDef, JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK,
    JS_PROP_ENUMERABLE, JSResult, JSValue, NativeObject, Object, ScopedContext,
    builtins::promise::Promise, def_native_object,
};

use crate::{aike::AikeRuntimeState, http::IncomingRequest};

use super::{
    CommandArgs, EnvVal,
    class_mcp::{
        JSBaseMcp, McpServicePayload, McpTransport, get_payload, js_str, parse_mcp_define,
    },
    service::{McpService, handle_mcp_tool_call},
};

struct StdioMcpShutdown {
    /// Handle to the running rmcp service. Taken by waiting() on shutdown.
    running_service: Mutex<Option<rmcp::service::RunningService<RoleServer, McpService>>>,
    /// Cancellation token, created up front (before handshake) so stop()/SIGTERM
    /// can cancel even while `serve_server_with_ct` is still handshaking.
    cancellation_token: Mutex<Option<CancellationToken>>,
    /// True while the server is actively serving.
    is_running: AtomicBool,
}

impl StdioMcpShutdown {
    /// Reset to the stopped/clean state: clear the server handle and the
    /// cancellation token, and flip `is_running` to false. Call on every exit
    /// path so a later `start()` begins from a clean slate and `isRunning()`/
    /// `stop()` report the correct stopped state.
    fn clear(&self) {
        self.is_running.store(false, Ordering::Release);
        *self.running_service.lock().unwrap() = None;
        *self.cancellation_token.lock().unwrap() = None;
    }

    /// Record the running server handle. Call after a successful
    /// `serve_server_with_ct` (Step 2) before entering the serve loop.
    /// The cancellation token is stored earlier, before the handshake begins.
    fn set_running(&self, running: rmcp::service::RunningService<RoleServer, McpService>) {
        self.is_running.store(true, Ordering::Release);
        *self.running_service.lock().unwrap() = Some(running);
    }
}

/// Shutdown control state in static — only Send+Sync types.
/// OnceLock is set once by start() and never released.
static MCP_STDIO_STATE: OnceLock<StdioMcpShutdown> = OnceLock::new();

/// The `StdioMcp` / `HttpMcp` payload type (`McpServicePayload`) and its
/// `McpTransport` enum now live in `service.rs` (merged from the old
/// `StdioMcpPayload` + `McpEndpoint`, see mcp-service.md §3).
///
/// Constructor callback — `StdioMcp` is NOT constructable. Instances must be
/// created via the static `StdioMcp.define(name, tools, options)`. Calling it
/// with `new` (or as a plain function) throws a TypeError.
fn js_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx
        .throw_type_error("StdioMcp cannot be constructed with `new`; use StdioMcp.define(config)"))
}

/// StdioMcp.define(config) — static factory taking a single config object.
fn js_stdio_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let Some(cfg) = cc.arg(0).as_object() else {
        return Err(ctx.throw_type_error("StdioMcp.define(config) — config must be an object"));
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
        && let Some(o) = v.as_object()
    {
        command_env = parse_env_object(ctx, o)?;
    }

    // args — string[] | (() => string[]).
    let mut command_args = CommandArgs::Empty;
    if let Some(v) = cfg
        .get_property_str(ctx, "args")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        command_args = if v.as_object().is_some_and(|o| o.is_callable()) {
            CommandArgs::Func(v.ensure_callable(ctx)?)
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
        before_run = Some(v.ensure_callable(ctx)?);
    }

    // extraFiles — string[].
    let mut extra_files = Vec::new();
    if let Some(v) = cfg
        .get_property_str(ctx, "extraFiles")
        .ok()
        .filter(|v| !v.is_undefined())
        && let Some(arr) = v.as_object()
    {
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
                    lst.push(js_str(ctx, f.clone())?);
                }
                Ok(Array::from_values(ctx, lst)?.into())
            }
            _ => Ok(Array::with_capacity(ctx, 0)?.into()),
        },
        FIELD_BEFORE_RUN => match &*p.transport.borrow() {
            McpTransport::Stdio { before_run, .. } => match before_run {
                None => Ok(JSValue::NULL),
                Some(f) => Ok((*f).into()),
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
    if let McpTransport::Stdio { before_run, .. } = &*p.transport.borrow()
        && let Some(hook) = before_run
        && let Err(e) = hook.call(ctx, this, &[])
    {
        return Err(e);
    }

    // Launch the stdio server
    let mut promise = start_stdio_server(ctx, p)?;

    // Stop on SIGINT / SIGTERM.
    {
        let stop_fn: JSValue = ctx
            .create_native_function(Callback::Func(js_stop), "mcpStop", 0, 0)?
            .into();

        crate::aike::register_signal_handler(ctx, "SIGINT", stop_fn)?;
        crate::aike::register_signal_handler(ctx, "SIGTERM", stop_fn)?;
    }

    // Force process exit once the server stops (via `run`'s `.then`). All MCP
    // cleanup has already happened — the promise only resolves after the
    // background server task finishes. Set the runtime interrupt flag (NOT
    // `std::process::exit`, which would skip GC finalization and leak nodes);
    // `main.rs` detects it and exits through the normal path.
    let exit_fn = ctx.create_native_function(
        Callback::Func(|ctx: &mut ScopedContext, _| {
            ctx.rt_mut().interrupt(ExitCode::SUCCESS);
            Ok(JSValue::UNDEFINED)
        }),
        "exit",
        0,
        0,
    )?;
    let noop =
        ctx.create_native_function(Callback::Func(|_, _| Ok(JSValue::UNDEFINED)), "noop", 0, 0)?;
    let _ = promise.then(ctx, exit_fn.into(), noop.into())?;

    Ok(promise.into())
}

/// Resolve `spawnEnv` / `spawnArgs` against the agent's context.
fn js_spawn_resolve(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    // ns arg — the agent's McpCtx (Record<string,string>); default to {}.
    let nsobj = match cc.arg(0).as_object() {
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
                        EnvVal::Text(s) => js_str(ctx, s.clone())?,
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
                        lst.push(js_str(ctx, s.clone())?);
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
        return Err(ctx.throw_internal_error(
            "Has running stdio mcp server — it can only be started once per process.",
        ));
    }

    // Route all console.log output to stderr to keep stdout clean for MCP protocol.
    // Refuse if stdio is already captured by another (concurrent) server.
    if let Err(msg) = crate::repl::caupture_stdio(true) {
        return Err(ctx.throw_type_error(msg.as_str()));
    }

    // Read compiled JS handlers produced at define() time.
    let js_handlers = p.js_handlers.borrow().clone();

    // Create crossbeam channel (same IncomingRequest mechanism as HTTP mode)
    let (js_tx, js_rx) = ctx.make_local_channel::<IncomingRequest>();
    // Build McpService from payload (stdio: no native handlers, single endpoint).
    let service = McpService::from_payload(p, js_tx, 0);

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
            move |_| b.load(Ordering::Acquire)
        },
        |_| {},
    );

    // Create Promise that resolves when server stops
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_, _| Ok(JSValue::UNDEFINED))?;

    // Store js_handlers on the runtime state for GC tracing (Object is !Send).
    AikeRuntimeState::of(ctx).stdio_js_handlers = js_handlers;

    // Initialize shutdown state (only Send+Sync fields in static)
    MCP_STDIO_STATE
        .set(StdioMcpShutdown {
            running_service: Mutex::new(None),
            cancellation_token: Mutex::new(None),
            is_running: AtomicBool::new(true),
        })
        .ok();

    // Spawn background tokio task to run rmcp serve_server_with_ct
    tokio::spawn(async move {
        // Create the cancellation token up front — before the handshake — so
        // stop()/SIGINT/SIGTERM can cancel even while serve_server_with_ct is
        // still handshaking. The same token is handed to serve_server_with_ct
        // and lives on in RunningService, so cancelling it works at any stage.
        let ct = CancellationToken::new();
        if let Some(state) = MCP_STDIO_STATE.get() {
            *state.cancellation_token.lock().unwrap() = Some(ct.clone());
        }

        // Close fd 0 on shutdown so the transport's blocking stdin read
        // (on tokio's blocking pool, not cancellable by the token) returns and
        // the runtime can tear down. See cleanup blocks below for rationale.
        let close_stdin = || {
            #[cfg(unix)]
            let _ = unsafe { libc::close(libc::STDIN_FILENO) };
        };

        let transport = (tokio::io::stdin(), tokio::io::stdout());

        // Step 1: Initialize (handshake). serve_server_with_ct returns quickly
        // with Cancelled if ct is cancelled mid-handshake, so SIGTERM during
        // the handshake no longer hangs.
        let running = match serve_server_with_ct(service, transport, ct.clone()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[StdioMcp] init error: {:?}", e);

                // Fully clear running state so a later start() gets a clean slate.
                if let Some(state) = MCP_STDIO_STATE.get() {
                    state.clear();
                }

                is_running.store(false, Ordering::Release);
                let _ = crate::repl::caupture_stdio(false);
                close_stdin();
                let _ = tx.send_async(Ok(())).await;
                return;
            }
        };

        // Step 2: Store RunningService handle (token already stored above).
        if let Some(state) = MCP_STDIO_STATE.get() {
            state.set_running(running);
        }

        // Step 3: Wait for termination (natural EOF, cancel, or error).
        // r.waiting().await returns when rmcp sees stdin EOF, or when the
        // cancellation token fires (SIGTERM/SIGINT/stop).
        if let Some(r) = MCP_STDIO_STATE
            .get()
            .and_then(|s| s.running_service.lock().unwrap().take())
        {
            let _ = r.waiting().await;
        }

        // Step 4: Cleanup — fully reset running state and resolve Promise.
        if let Some(state) = MCP_STDIO_STATE.get() {
            state.clear();
        }
        is_running.store(false, Ordering::Release);
        let _ = crate::repl::caupture_stdio(false);

        // Unblock the runtime teardown. rmcp's stdio transport reads stdin via
        // tokio's blocking pool (a synchronous read(fd 0) on a worker thread),
        // which is NOT cancelled by the cancellation token. If the parent still
        // holds the stdin write end (no EOF), that read never returns and
        // tokio's BlockingPool::shutdown hangs forever — the process never
        // exits. Closing fd 0 makes the in-flight read return, so the blocking
        // task ends and the runtime can shut down. Safe here: stdin is the MCP
        // protocol and the process is exiting anyway.
        close_stdin();
        let _ = tx.send_async(Ok(())).await;
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

    // Take the cancellation token (created before the handshake) and fire it.
    // Works whether the server is mid-handshake or already serving.
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

        let val = if v.as_object().is_some_and(|o| o.is_callable()) {
            EnvVal::Func(v.ensure_callable(ctx)?)
        } else {
            let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
            EnvVal::Text(s)
        };
        settings.insert(name, val);
    }

    Ok(settings)
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
    extends: JSBaseMcp::class_id
);
