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
use rmcp::service::RoleServer;
use zuqe::{
    Array, CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JS_PROP_ENUMERABLE, JSResult,
    JSValue, Object, PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, http::IncomingRequest};

use super::{
    ArgsVal, EnvVal,
    class_mcp_server_def::{parse_env_object, register_signal},
    service::{
        McpSchemas, McpService, McpServicePayload, McpTransport, handle_mcp_tool_call,
        parse_mcp_tools,
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

/// Create a JS string value, taking ownership of `s` so no extra allocation
/// is needed when the caller already holds a `String`.
fn js_str(ctx: &mut ScopedContext, s: String) -> JSResult {
    Ok(ctx.create_string_from_utf8(s.into_bytes())?.into())
}

/// Resolve the shared `McpServicePayload` from either a `JSStdioMcp` or a
/// `JSHttpMcp` instance (both wrap the same payload type).
fn get_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a McpServicePayload, i32> {
    if let Some(o) = val.as_native_object::<JSStdioMcp>() {
        if let Some(p) = o.payload() {
            return Ok(p);
        }
    }
    if let Some(o) = val.as_native_object::<JSHttpMcp>() {
        if let Some(p) = o.payload() {
            return Ok(p);
        }
    }
    ctx.throw_type_error("not a Mcp server instance");
    Err(-1)
}

/// Mutable variant of `get_payload` — returns the payload for mutation
/// (e.g. dynamically adding tools).
fn get_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut McpServicePayload, i32> {
    // Peek the class via immutable borrows first (repeatable), then take a
    // single mutable borrow inside the matching branch. This avoids two
    // sequential `&mut` borrows of `val`, which the borrow checker rejects
    // (the `&'a mut` return would otherwise tie `val` as borrowed for `'a`
    // across the whole function).
    let is_stdio = val.as_native_object::<JSStdioMcp>().is_some();
    let is_http = val.as_native_object::<JSHttpMcp>().is_some();
    if is_stdio {
        return val
            .as_native_object_mut::<JSStdioMcp>()
            .and_then(|o| o.payload_mut())
            .ok_or_else(|| {
                ctx.throw_type_error("not a Mcp server instance");
                -1
            });
    }
    if is_http {
        return val
            .as_native_object_mut::<JSHttpMcp>()
            .and_then(|o| o.payload_mut())
            .ok_or_else(|| {
                ctx.throw_type_error("not a Mcp server instance");
                -1
            });
    }
    ctx.throw_type_error("not a Mcp server instance");
    Err(-1)
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
/// carries version / instructions / spawnEnv / spawnArgs / beforeRun / shared /
/// extraFiles (everything except `tools`, which is lifted to its own arg).
fn js_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    // tools — required second argument (object-keyed DSL or array).
    let tools_val = cc.arg(1);
    if tools_val.as_object().is_none() {
        ctx.throw_type_error("`tools` must be an array or object");
        return Err(-1);
    }

    let mut payload = McpServicePayload::new_stdio(name);

    // Optional third `options` object (everything except tools).
    let opt = cc.arg(2);
    if !opt.is_undefined() {
        let Some(obj) = opt.as_object().copied() else {
            ctx.throw_type_error("StdioMcp.define options must be an object");
            return Err(-1);
        };
        apply_stdio_options(ctx, &mut payload, obj)?;
    }

    // Shared parser: write compiled schemas + js_handlers into the payload.
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;
    payload.schemas = Arc::new(RwLock::new(McpSchemas {
        tools,
        name_to_index,
        native_map: HashMap::new(),
    }));
    payload.js_handlers = RefCell::new(js_handlers);

    JSStdioMcp::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

const FIELD_VERSION: i16 = 0;
const FIELD_INSTRUCTIONS: i16 = 1;
const FIELD_NAME: i16 = 3;
const FIELD_SHARED: i16 = 6;
const FIELD_EXTRA_FILES: i16 = 8;
const FIELD_BEFORE_RUN: i16 = 9;
const FIELD_SPAWN_ENV: i16 = 10;
const FIELD_SPAWN_ARGS: i16 = 11;

fn js_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;

    match cc.magic() as i16 {
        FIELD_NAME => js_str(ctx, p.server_name.clone()),
        FIELD_VERSION => js_str(ctx, p.version.borrow().to_string()),
        FIELD_INSTRUCTIONS => js_str(ctx, p.instructions.borrow().to_string()),
        FIELD_SHARED => match &p.transport {
            McpTransport::Stdio { is_shared, .. } => Ok(JSValue::from(is_shared.get())),
            _ => Ok(JSValue::from(false)),
        },
        FIELD_EXTRA_FILES => match &p.transport {
            McpTransport::Stdio { extra_files, .. } => {
                let files: Vec<String> = extra_files.borrow().clone();
                let mut lst = Vec::with_capacity(files.len());
                for f in &files {
                    lst.push(js_str(ctx, f.clone())?.into());
                }
                Ok(Array::from_values(ctx, lst)?.into())
            }
            _ => Ok(Array::with_capacity(ctx, 0)?.into()),
        },
        FIELD_BEFORE_RUN => match &p.transport {
            McpTransport::Stdio { before_run, .. } => match &*before_run.borrow() {
                None => Ok(JSValue::NULL),
                Some(f) => Ok(f.clone().into()),
            },
            _ => Ok(JSValue::NULL),
        },
        FIELD_SPAWN_ENV => match &p.transport {
            McpTransport::Stdio { spawn_env, .. } => {
                // Build { key: string | (() => string | undefined) } from stored EnvVal map.
                let mut obj = ctx.create_base_object()?;
                for (k, v) in spawn_env.borrow().iter() {
                    let val = match v {
                        EnvVal::Text(s) => js_str(ctx, s.clone())?.into(),
                        EnvVal::Func(f) => f.clone().into(),
                    };
                    obj.define_property_value_str(ctx, k, val, JS_PROP_ENUMERABLE)?;
                }
                Ok(obj.into())
            }
            _ => Ok(ctx.create_base_object()?.into()),
        },
        FIELD_SPAWN_ARGS => match &p.transport {
            McpTransport::Stdio { spawn_args, .. } => match &*spawn_args.borrow() {
                None => Ok(JSValue::UNDEFINED),
                Some(ArgsVal::Text(v)) => {
                    let mut lst = Vec::with_capacity(v.len());
                    for s in v {
                        lst.push(js_str(ctx, s.clone())?.into());
                    }
                    Ok(Array::from_values(ctx, lst)?.into())
                }
                Some(ArgsVal::Func(f)) => Ok(f.clone().into()),
            },
            _ => Ok(JSValue::UNDEFINED),
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

    // beforeRun — user init hook, run once before serving (memo/mcp-service.md §2.2).
    // On the JS thread here; fire-and-forget (async hook may still be pending when
    // serve starts — awaiting a JS Promise would block this thread).
    if let McpTransport::Stdio { before_run, .. } = &p.transport {
        if let Some(hook) = before_run.borrow().clone() {
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

/// Apply the `StdioMcp.define(name, tools, options)` options object onto a
/// fresh payload. `tools` is handled separately (it is the lifted 2nd arg),
/// so this covers version / instructions / spawnEnv / spawnArgs / beforeRun / shared /
/// extraFiles.
fn apply_stdio_options(
    ctx: &mut ScopedContext,
    p: &mut McpServicePayload,
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

    // spawnEnv — Record<string, string | (() => string | undefined)>.
    // Forwarded to Aike.spawn when the framework launches this server process.
    if let Some(v) = opt
        .get_property_str(ctx, "spawnEnv")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        if let Some(obj) = v.as_object().copied() {
            if let McpTransport::Stdio { spawn_env, .. } = &mut p.transport {
                *spawn_env.borrow_mut() = parse_env_object(ctx, obj)?;
            }
        }
    }

    // spawnArgs — string[] | (() => string[]).
    // e.g. ["--stack-overflow", "1m"]; forwarded to Aike.spawn as CLI args.
    if let Some(v) = opt
        .get_property_str(ctx, "spawnArgs")
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
        if let McpTransport::Stdio { spawn_args, .. } = &mut p.transport {
            *spawn_args.borrow_mut() = Some(val);
        }
    }

    // beforeRun — () => Promise<void>.
    if let Some(v) = opt
        .get_property_str(ctx, "beforeRun")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let func = v.into_callable(ctx)?;
        if let McpTransport::Stdio { before_run, .. } = &mut p.transport {
            *before_run.borrow_mut() = Some(func);
        }
    }

    // shared — boolean.
    if let Some(v) = opt
        .get_property_str(ctx, "shared")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let b = v.to_bool().unwrap_or(false);
        if let McpTransport::Stdio { is_shared, .. } = &mut p.transport {
            is_shared.set(b);
        }
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
            if let McpTransport::Stdio { extra_files, .. } = &mut p.transport {
                *extra_files.borrow_mut() = paths;
            }
        }
    }

    Ok(())
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

// ── Shared instance getters (same for JSStdioMcp and JSHttpMcp) ──

fn js_name(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.server_name.clone())
}

fn js_version(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.version.borrow().to_string())
}

fn js_instructions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    js_str(ctx, p.instructions.borrow().to_string())
}

/// Dynamically add JS tools after construction. Accepts the same `tools` DSL as
/// `define` (object-keyed map or `McpToolDefinition[]` array). For an HTTP server
/// that is already listening, the new handler table is pushed through the
/// hot-reload channel so it takes effect on the next request. For stdio, call
/// BEFORE `run()` (the server is built from the payload at run time).
fn js_add_tool(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_payload_mut(ctx, &mut this)?;

    let tools_val = cc.arg(0);
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    let new_handlers = p.register_js_tools(tools, name_to_index, js_handlers);

    // HTTP hot-reload: push the new handler table so the running poller picks it up.
    if let McpTransport::Http { update_tx, .. } = &p.transport {
        if let Some(tx) = update_tx {
            let _ = tx.send(new_handlers);
        }
    }

    Ok(this)
}

/// `HttpMcp.updateTools(tools)` — hot-reload: REPLACE all JS tools (native
/// tools are preserved). For an HTTP server that is already listening, the
/// new handler table is pushed through the hot-reload channel so it takes
/// effect on the next request.
fn js_http_update_tools(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_payload_mut(ctx, &mut this)?;

    // HttpMcp is always an Http transport, but guard for safety.
    match &p.transport {
        McpTransport::Http { .. } => {}
        _ => {
            ctx.throw_type_error("updateTools is only supported on an Http transport");
            return Err(-1);
        }
    }

    // arg(0): tools (array or object-keyed format)
    let tools_val = cc.arg(0);
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    // Replace all JS tools, preserving native tool entries.
    {
        let mut guard = p.schemas.write().unwrap();
        let native_names: Vec<String> = guard.native_map.keys().cloned().collect();
        let mut new_tools = tools;
        let mut new_name_to_index = name_to_index;
        let mut new_native_map = HashMap::new();

        for name in &native_names {
            if let Some(&old_idx) = guard.name_to_index.get(name) {
                let new_idx = new_tools.len();
                new_tools.push(guard.tools[old_idx].clone());
                new_name_to_index.insert(name.clone(), new_idx);
                if let Some(&ni) = guard.native_map.get(name) {
                    new_native_map.insert(name.clone(), ni);
                }
            }
        }

        guard.tools = new_tools;
        guard.name_to_index = new_name_to_index;
        guard.native_map = new_native_map;
    }

    // Update JS handlers on payload (for GC tracing).
    p.js_handlers = RefCell::new(js_handlers.clone());

    // Send new handler table to the poller via update channel.
    if let McpTransport::Http { update_tx, .. } = &p.transport {
        if let Some(utx) = update_tx {
            let _ = utx.send(js_handlers);
        }
    }

    Ok(this)
}

// ── JSStdioMcp ──

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
    FieldDef::GETSET("spawnEnv", Some(js_get), None, FIELD_SPAWN_ENV),
    FieldDef::GETSET("spawnArgs", Some(js_get), None, FIELD_SPAWN_ARGS),
    FieldDef::FUNC("addTool", 1, js_add_tool, 0),
    FieldDef::FUNC("run", 0, js_run, 0),
    FieldDef::GETSET("name", Some(js_name), None, 0),
    FieldDef::GETSET("version", Some(js_version), None, 0),
    FieldDef::GETSET("instructions", Some(js_instructions), None, 0),
];

def_native_object!(
    "StdioMcp",
    JSStdioMcp,
    McpServicePayload,
    STDIO_MCP_PROTO,
    Some(STDIO_MCP_CTOR),
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_stdio_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSStdioMcp::register(rt)
}

const FIELD_MOUNT_PATH: i16 = 12;

/// `HttpMcp.define(name, tools, options)` — static factory.
///
/// `name` is the advertised server name (required, same position as
/// `StdioMcp.define`). `tools` is the required second argument (object-keyed
/// DSL or array). `options` carries version / instructions (everything except
/// `tools`). The compiled schemas + handlers are written into the payload at
/// define() time. The mount path is NOT set here — it is assigned by
/// `HttpServer.mcp(mountPath, server)` at mount time.
fn js_http_define(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // name — required first argument (advertised server name).
    let name = cc.arg(0).to_string(ctx).map_err(|_| {
        ctx.throw_type_error("HttpMcp.define name must be a string");
        -1
    })?;
    let name = name.content().encode_utf8(false).to_string();

    // tools — required second argument (object-keyed DSL or array).
    let tools_val = cc.arg(1);
    if tools_val.as_object().is_none() {
        ctx.throw_type_error("`tools` must be an array or object");
        return Err(-1);
    }

    let mut payload = McpServicePayload::new_http();
    payload.server_name = name;

    // Optional third `options` object (version / instructions).
    let opt = cc.arg(2);
    if !opt.is_undefined() {
        let Some(obj) = opt.as_object().copied() else {
            ctx.throw_type_error("HttpMcp.define options must be an object");
            return Err(-1);
        };
        apply_http_options(ctx, &mut payload, obj)?;
    }

    // Shared parser: write compiled schemas + js_handlers into the payload.
    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;
    payload.schemas = Arc::new(RwLock::new(McpSchemas {
        tools,
        name_to_index,
        native_map: HashMap::new(),
    }));
    payload.js_handlers = RefCell::new(js_handlers);

    JSHttpMcp::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

/// Apply the `HttpMcp.define(name, tools, options)` options object onto a
/// fresh payload. Covers version / instructions (serverName is positional).
fn apply_http_options(
    ctx: &mut ScopedContext,
    p: &mut McpServicePayload,
    opt: Object,
) -> Result<(), i32> {
    if let Some(v) = opt
        .get_property_str(ctx, "version")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *p.version.borrow_mut() = Cow::Owned(s);
    }
    if let Some(v) = opt
        .get_property_str(ctx, "instructions")
        .ok()
        .filter(|v| !v.is_undefined())
    {
        let s = v.to_string(ctx)?.content().encode_utf8(false).to_string();
        *p.instructions.borrow_mut() = Cow::Owned(s);
    }

    Ok(())
}

fn js_http_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error(
        "HttpMcp cannot be constructed with `new`; use HttpMcp.define(name, tools, options)",
    );
    Err(-1)
}

/// http-only getter for `mountPath`.
fn js_http_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_payload(ctx, &this)?;
    match cc.magic() as i16 {
        FIELD_MOUNT_PATH => match &p.transport {
            McpTransport::Http { mount_path, .. } => js_str(ctx, mount_path.clone()),
            _ => Ok(JSValue::UNDEFINED),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

const HTTP_MCP_STATIC_ENTRIES: &[FieldDef] = &[FieldDef::FUNC("define", 3, js_http_define, 0)];

const HTTP_MCP_CTOR: ClassCtorDef =
    ClassCtorDef::new(js_http_constructor, 1, true).entries(HTTP_MCP_STATIC_ENTRIES);

const HTTP_MCP_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("initializeInfo", Some(js_initialize_info), None, 0),
    FieldDef::GETSET("toolsList", Some(js_tools_list), Some(js_tools_list), 0),
    FieldDef::GETSET("name", Some(js_name), None, 0),
    FieldDef::GETSET("version", Some(js_version), None, 0),
    FieldDef::GETSET("instructions", Some(js_instructions), None, 0),
    FieldDef::GETSET("mountPath", Some(js_http_get), None, FIELD_MOUNT_PATH),
    FieldDef::FUNC("addTool", 1, js_add_tool, 0),
    FieldDef::FUNC("updateTools", 1, js_http_update_tools, 0),
];

def_native_object!(
    "HttpMcp",
    JSHttpMcp,
    McpServicePayload,
    HTTP_MCP_PROTO,
    Some(HTTP_MCP_CTOR),
    on_payload_gc: McpServicePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_mcp(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpMcp::register(rt)
}
