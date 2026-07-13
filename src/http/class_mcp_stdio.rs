// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

// JS binding for McpStdio — singleton MCP server over stdio transport.
//
// McpStdio is a pure-static singleton class: all methods are static,
// no instances can be created. stdin/stdout is a process-wide exclusive
// resource, so only one McpStdio server can run per process.
//
// Lifecycle: start() → [serving] → stop() or stdin EOF → [stopped]
// start() can only be called once; OnceLock is never released.

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
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, PollSender, Runtime,
    ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::{
    class_http_server::{IncomingRequest, parse_mcp_tools},
    mcp::{McpEndpoint, McpSchemas, McpService, handle_mcp_tool_call},
};

/// Shutdown control state in static — only Send+Sync types.
/// OnceLock is set once by start() and never released.
static MCP_STDIO_STATE: OnceLock<McpStdioShutdown> = OnceLock::new();

struct McpStdioShutdown {
    /// Handle to the running rmcp service. Taken by waiting() on shutdown.
    running_service: Mutex<Option<rmcp::service::RunningService<RoleServer, McpService>>>,
    /// Cancellation token for sync stop signal.
    cancellation_token: Mutex<Option<rmcp::service::RunningServiceCancellationToken>>,
    /// True while the server is actively serving.
    is_running: AtomicBool,
}

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

fn js_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error(
        "McpStdio is a singleton. Use McpStdio.start(options) instead of new McpStdio().",
    );
    Err(-1)
}

/// McpStdio.start(options) — start the stdio MCP server.
///
/// Returns a Promise<void> that resolves when the server stops
/// (via stop(), stdin EOF, or error). Non-blocking.
fn js_start(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // 1. Refuse if already started (even if stopped — OnceLock is permanent)
    if MCP_STDIO_STATE.get().is_some() {
        ctx.throw_type_error(
            "McpStdio.start() has already been called. \
             McpStdio is a singleton — it can only be started once per process.",
        );
        return Err(-1);
    }

    // Route all console.log output to stderr to keep stdout clean for MCP protocol
    crate::repl::set_stdio_mode();

    // 2. Parse options from args[0]
    let opts = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| {
            ctx.throw_type_error("options must be an object");
            -1
        })?
        .clone();

    let server_name = opts
        .get_property_str(ctx, "serverName")
        .and_then(|v| v.to_string(ctx))
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|_| "mcp-server".into());

    let server_version = opts
        .get_property_str(ctx, "version")
        .and_then(|v| v.to_string(ctx))
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|_| "1.0.0".into());

    // tools (array or object-keyed format)
    let tools_val = opts.get_property_str(ctx, "tools").map_err(|_| {
        ctx.throw_type_error("options.tools is required");
        -1
    })?;

    let (tools, name_to_index, js_handlers) = parse_mcp_tools(ctx, &tools_val)?;

    // 3. Build McpEndpoint (stdio mode: no mount_path, no update_tx)
    let endpoint = McpEndpoint {
        mount_path: String::new(),
        server_name,
        server_version,
        shared: std::sync::Arc::new(std::sync::RwLock::new(McpSchemas {
            tools,
            name_to_index,
            native_map: HashMap::new(),
        })),
        js_handlers,
        handlers: Vec::new(),
        update_tx: None,
    };

    // 4. Create crossbeam channel (same IncomingRequest mechanism as HTTP mode)
    let (js_tx, js_rx) = channel::unbounded::<IncomingRequest>();

    // 5. Build McpService (reuse mcp.rs)
    let service = McpService::from_endpoint(&endpoint, js_tx, 0);

    // 6. Start JS poller job for tool call dispatch
    let mcp_tables = RefCell::new(vec![endpoint.js_handlers.clone()]);
    let is_running_poller = std::sync::Arc::new(AtomicBool::new(true));
    let is_running_poller2 = is_running_poller.clone();

    crate::helpers::start_channel_poller(
        ctx,
        "stdio_mcp",
        js_rx,
        move |incoming: IncomingRequest, cx| {
            match incoming {
                IncomingRequest::Http(_) => {} // should never happen in stdio mode
                IncomingRequest::McpToolCall(req) => {
                    handle_mcp_tool_call(cx, req, &mcp_tables.borrow());
                }
                // WS is HTTP-server-only; never produced in stdio MCP mode.
                IncomingRequest::WebSocketOpen { .. } | IncomingRequest::WebSocketData { .. } => {}
            }
            true
        },
        move || is_running_poller2.load(Ordering::Acquire),
        || {},
    );

    // 7. Create Promise that resolves when server stops
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;

    // 8. Store js_handlers in thread_local for GC tracing (Object is !Send)
    STDIO_JS_HANDLERS.with(|h| {
        *h.borrow_mut() = endpoint.js_handlers.clone();
    });

    // 9. Initialize shutdown state (only Send+Sync fields in static)
    let state = McpStdioShutdown {
        running_service: Mutex::new(None),
        cancellation_token: Mutex::new(None),
        is_running: AtomicBool::new(true),
    };
    // Safe: we checked is_some() above and this is the only write path.
    MCP_STDIO_STATE.set(state).ok();

    // 9. Spawn background tokio task to run rmcp serve_server
    tokio::spawn(async move {
        let transport = (tokio::io::stdin(), tokio::io::stdout());

        // Step 1: Initialize (handshake) — returns quickly on error or cancel
        let running = match rmcp::serve_server(service, transport).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[McpStdio] init error: {:?}", e);
                if let Some(state) = MCP_STDIO_STATE.get() {
                    state.is_running.store(false, Ordering::Release);
                    // Also stop the poller
                    // (is_running_poller is the poller's keep-alive flag)
                }
                is_running_poller.store(false, Ordering::Release);
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
        is_running_poller.store(false, Ordering::Release);
        let _ = PollSender::from(tx).send_async(Ok(())).await;
    });

    Ok(promise.into())
}

/// McpStdio.stop() — gracefully stop the stdio MCP server.
///
/// Cancels the rmcp service via its cancellation token (sync).
/// The background task will detect cancellation via waiting() and
/// resolve the Promise returned by start().
/// Safe to call multiple times; no-op if already stopped.
fn js_stop(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let state = match MCP_STDIO_STATE.get() {
        Some(s) => s,
        None => return Ok(JSValue::UNDEFINED), // never started
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

/// McpStdio.isRunning() — returns true if the server is currently serving.
fn js_is_running(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    match MCP_STDIO_STATE.get() {
        Some(state) => Ok(JSValue::from(state.is_running.load(Ordering::Acquire))),
        None => Ok(JSValue::from(false)),
    }
}

// Static methods defined on the class constructor (not prototype)
const MCP_STDIO_STATIC_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("start", 1, js_start, 0),
    FieldDef::FUNC("stop", 0, js_stop, 0),
    FieldDef::FUNC("isRunning", 0, js_is_running, 0),
];

// Empty proto — McpStdio has no instance methods
const MCP_STDIO_PROTO: &[FieldDef] = &[];

// Zero-sized payload (constructor always throws, so no instance is ever created).
// Uses with-payload macro path to avoid edition 2024 metavar forwarding issues.
struct McpStdioPayload;

def_native_object!(
    "McpStdio",
    JSMcpStdio,
    McpStdioPayload,
    MCP_STDIO_PROTO,
    Some(ClassCtorDef::new(js_constructor, 0, false).entries(MCP_STDIO_STATIC_ENTRIES))
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_mcp_stdio(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSMcpStdio::register(rt)
}
