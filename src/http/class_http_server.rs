// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::{FromRequestParts as _, RawPathParams, Request, ws::WebSocketUpgrade},
    response::{
        IntoResponse, Response as AxumResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{MethodRouter, delete, get, head, options, patch, post, put},
};
use crossbeam::channel;
use linkme::distributed_slice;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::services::ServeDir;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, Object,
    PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    mcp::{
        JSHttpMcp, McpToolCallRequest, McpTransport, build_axum_mcp_route, handle_mcp_tool_call,
    },
};

#[cfg(feature = "llama_cpp")]
use crate::llm::{
    JSEngine,
    engine::{Engine, serve::engine_openai_router},
};

use super::{
    JSWebSocketStream, Method,
    class_request::{HttpRequestPayload, JSHttpRequest},
    outbound_buffer::OUTBOUND_CAP,
    response::{HttpResponseMessage, HttpResponsePayload, JSHttpResponse, get_res_payload},
    server::spawn_http_server,
    ws::{WsControl, WsStreamEvent, bridge_ws, dispatch_ws_data, dispatch_ws_open},
};
/// Unified request message sent from axum delegate to the JS server job.
pub(crate) enum IncomingRequest {
    Http(HttpRequest),
    McpToolCall(McpToolCallRequest),
    /// A fresh WebSocket connection was upgraded; the JS poller must build the
    /// `WebSocketStream` and invoke the user's `ws()` handler.
    WebSocketOpen {
        handler_index: usize,
        id: u64,
        out_tx: mpsc::Sender<String>,
        control_tx: mpsc::Sender<WsControl>,
        query: String,
        params: HashMap<String, String>,
    },
    /// An inbound event for an existing WS connection (client message / close).
    WebSocketData {
        id: u64,
        event: WsStreamEvent,
    },
}

/// HTTP route request (existing logic, fields unchanged).
pub(crate) struct HttpRequest {
    handler_index: usize,
    method: Method,
    path: String,
    query_string: String,
    params: HashMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    response_tx: oneshot::Sender<HttpResponseMessage>,
}

pub(super) struct HttpServerPayload {
    js_routes: Vec<(Method, String, Object)>,
    mcp_endpoints: Vec<Object>,
    /// WebSocket routes registered via `server.ws(path, handler)`.
    ws_routes: Vec<(String, Object)>,
    /// Static file mounts registered via `server.staticFiles(mount, dir)`.
    static_dirs: Vec<(String, String)>,

    #[cfg(feature = "llama_cpp")]
    openai: Option<(Arc<std::sync::Mutex<Engine>>, String)>,

    shutdown_tx: Option<oneshot::Sender<()>>,
    is_running: Arc<AtomicBool>,
    frozen: bool,

    /// Live WebSocket connections keyed by stream id (for dispatch/cleanup).
    pub(crate) stream_registry: RefCell<HashMap<u64, Object>>,
    /// Topic → set of subscribed stream ids (Bun pub/sub).
    pub(crate) topic_registry: RefCell<HashMap<String, HashSet<u64>>>,
}

impl HttpServerPayload {
    fn add_js_route(
        &mut self,
        method: Method,
        path: String,
        handler: Object,
    ) -> Result<(), &'static str> {
        if self.frozen {
            return Err("cannot add route while server is running");
        }
        // Replace existing entry for the same (method, path) to avoid duplicates
        if let Some(entry) = self
            .js_routes
            .iter_mut()
            .find(|(m, p, _)| m == &method && p == &path)
        {
            entry.2 = handler;
        } else {
            self.js_routes.push((method, path, handler));
        }
        Ok(())
    }

    fn remove_route(&mut self, method: Method, path: &str) -> Result<bool, &'static str> {
        if self.frozen {
            return Err("cannot remove route while server is running");
        }
        let before = self.js_routes.len();
        self.js_routes
            .retain(|(m, p, _)| !(m == &method && p == path));
        Ok(self.js_routes.len() < before)
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for (_, _, handler) in &self.js_routes {
            handler.trace(gcx);
        }
        for o in &self.mcp_endpoints {
            o.trace(gcx);
        }
        for (_, handler) in &self.ws_routes {
            handler.trace(gcx);
        }
        for v in self.stream_registry.borrow().values() {
            v.trace(gcx);
        }
    }
}

fn get_server_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a HttpServerPayload, i32> {
    val.as_native_object::<JSHttpServer>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("expect HttpServer instance");
            -1
        })
}

pub(crate) fn get_server_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HttpServerPayload, i32> {
    val.as_native_object_mut::<JSHttpServer>()
        .and_then(|o| o.payload_mut())
        .ok_or_else(|| {
            ctx.throw_type_error("expect HttpServer instance");
            -1
        })
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    JSHttpServer::new_instance(
        ctx,
        cc.this(),
        HttpServerPayload {
            js_routes: Vec::new(),
            mcp_endpoints: Vec::new(),
            ws_routes: Vec::new(),
            static_dirs: Vec::new(),

            #[cfg(feature = "llama_cpp")]
            openai: None,

            shutdown_tx: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frozen: false,

            stream_registry: RefCell::new(HashMap::new()),
            topic_registry: RefCell::new(HashMap::new()),
        },
    )
    .map(|v| v.into())
}

/// HttpServer.openai(engine, mountPath) — mount OpenAI-compatible routes.
///
/// `engine` must be an Engine instance (ownership is transferred).
/// `mountPath` must start with '/', e.g. "/v1".
/// Calling after listen() raises an error.
#[cfg(feature = "llama_cpp")]
fn js_openai(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // Validate engine argument
    let mut v = cc.arg(0);
    let engine = match v.as_native_object_mut::<JSEngine>() {
        Some(e) => match e.take_payload() {
            Some(payload) => *payload,
            None => {
                ctx.throw_type_error("Engine object has already been detached");
                return Err(-1);
            }
        },
        None => {
            ctx.throw_type_error("engine must be an Engine instance");
            return Err(-1);
        }
    };

    // Validate mountPath argument
    let mount_path = {
        let s = cc.arg(1).to_string(ctx).map_err(|_| {
            ctx.throw_type_error("mountPath must be a string");
            -1
        })?;
        let text = s.content().encode_utf8(false).to_string();
        if !text.starts_with('/') {
            ctx.throw_type_error("mountPath must start with '/'");
            return Err(-1);
        }
        text
    };

    let p = get_server_payload_mut(ctx, &mut this)?;
    if p.frozen {
        ctx.throw_type_error("cannot call openai() after listen()");
        return Err(-1);
    }
    p.openai = Some((engine, mount_path));

    Ok(this)
}

fn js_route_add(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let method = match cc.magic() as u8 {
        0 => Method::GET,
        1 => Method::POST,
        2 => Method::PUT,
        3 => Method::PATCH,
        4 => Method::DELETE,
        5 => Method::HEAD,
        6 => Method::OPTIONS,
        _ => {
            ctx.throw_internal_error("unknown method magic");
            return Err(-1);
        }
    };
    let path_str = cc.arg(0).to_string(ctx)?;
    let path = path_str.content().encode_utf8(false).to_string();
    let handler = cc.arg(1).into_callable(ctx)?;

    let p = get_server_payload_mut(ctx, &mut this)?;
    if let Err(e) = p.add_js_route(method, path, handler) {
        ctx.throw_type_error(e);
        return Err(-1);
    }
    Ok(this)
}

/// HttpServer.mcp(httpMcp) — register an `HttpMcp` instance at its `mountPath`.
///
/// The `HttpMcp` instance already carries the compiled tool registry and
/// `mountPath` (in its `Http` transport variant). We just store the JS handle
/// as the single source of truth; `listen()` reads it back to build the route
/// and backfills the hot-reload channel (mcp-service.md §6). Must be called
/// BEFORE `listen()`.
fn js_mcp(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // arg(0): mountPath — the URL the endpoint is mounted at (must start with '/').
    let mount_path = {
        let s = cc.arg(0).to_string(ctx).map_err(|_| {
            ctx.throw_type_error("mcp() mountPath must be a string");
            -1
        })?;
        let s = s.content().encode_utf8(false).to_string();
        if !s.starts_with('/') {
            ctx.throw_type_error("mcp() mountPath must start with '/'");
            return Err(-1);
        }
        s
    };

    // arg(1): an HttpMcp instance.
    let mut http_obj = cc.arg(1).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("mcp() expects an HttpMcp instance");
        -1
    })?;

    // Validate it is an HttpMcp with an Http transport, then assign the
    // mount path the server owns (not baked into the HttpMcp at define time).
    {
        let h = http_obj
            .as_native_object_mut::<JSHttpMcp>()
            .ok_or_else(|| {
                ctx.throw_type_error("mcp() expects an HttpMcp instance");
                -1
            })?;
        let p = h.payload_mut().ok_or_else(|| {
            ctx.throw_type_error("invalid HttpMcp instance");
            -1
        })?;
        match &mut *p.transport.borrow_mut() {
            McpTransport::Http { mount_path: mp, .. } => {
                *mp = mount_path;
            }
            _ => {
                ctx.throw_type_error("mcp() expects an Http (not stdio) Mcp server");
                return Err(-1);
            }
        }
    }

    let p = get_server_payload_mut(ctx, &mut this)?;
    if p.frozen {
        ctx.throw_type_error("cannot add mcp after listen()");
        return Err(-1);
    }

    p.mcp_endpoints.push(http_obj);
    Ok(this)
}

fn js_remove_route(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let method = match &*cc.arg(0).to_string(ctx)?.content().encode_utf8(false) {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "PUT" => Method::PUT,
        "PATCH" => Method::PATCH,
        "DELETE" => Method::DELETE,
        "HEAD" => Method::HEAD,
        "OPTIONS" => Method::OPTIONS,
        _ => {
            ctx.throw_type_error("unknown HTTP method");
            return Err(-1);
        }
    };
    let path_str = cc.arg(1).to_string(ctx)?;
    let path = path_str.content().encode_utf8(false).to_string();

    get_server_payload_mut(ctx, &mut this)?
        .remove_route(method, &path)
        .map_err(|e| {
            ctx.throw_type_error(e);
            -1
        })
        .map(JSValue::Bool)
}

/// Convert Express-style `:param` path segments to axum-style `{param}`.
fn express_to_axum_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' {
            let mut param = String::new();
            let mut pushed = false;
            for ch in chars.by_ref() {
                if ch.is_alphanumeric() || ch == '_' {
                    param.push(ch);
                } else {
                    result.push('{');
                    result.push_str(&param);
                    result.push('}');
                    result.push(ch);
                    pushed = true;
                    break;
                }
            }
            if !param.is_empty() && !pushed {
                result.push('{');
                result.push_str(&param);
                result.push('}');
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Validate axum path — reject segments that mix `{param}` with literal text.
/// This mirrors matchit's internal validation to avoid panics from router.route().
fn validate_axum_path(path: &str) -> Result<(), String> {
    if path.is_empty() || !path.starts_with('/') {
        return Err(format!(
            "Invalid route \"{path}\": path must start with '/'"
        ));
    }
    for segment in path.split('/') {
        // Check for mixed param + literal: segment like "{taskId}restore"
        if let Some(open) = segment.find('{') {
            // param-like segment — ensure no literal text before '{'
            if open > 0 {
                return Err(format!(
                    "Invalid route \"{path}\": Only one parameter is allowed per path segment"
                ));
            }
            if let Some(close) = segment.rfind('}') {
                // literal text after '}'
                if close + 1 < segment.len() {
                    return Err(format!(
                        "Invalid route \"{path}\": Only one parameter is allowed per path segment"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Create a (path, MethodRouter) pair that delegates to a JS handler via channel.
///
/// The closure captures only `handler_index: usize` and `Sender` — both Send + Sync.
/// JS Object stays in the JS thread handler lookup table.
fn delegate_js_route(
    method: Method,
    path: String,
    handler_index: usize,
    request_tx: channel::Sender<IncomingRequest>,
) -> (String, MethodRouter<()>) {
    let axum_path = express_to_axum_path(&path);
    let tx = request_tx;

    let handler_fn = move |req: Request| {
        let tx = tx.clone();

        async move {
            // Split request to access extensions (axum stores parsed path params there)
            let (mut parts, body) = req.into_parts();
            let (res_tx, res_rx) = oneshot::channel();
            let uri = parts.uri.clone();
            let method = match parts.method.as_str() {
                "GET" => Method::GET,
                "POST" => Method::POST,
                "PUT" => Method::PUT,
                "PATCH" => Method::PATCH,
                "DELETE" => Method::DELETE,
                "HEAD" => Method::HEAD,
                "OPTIONS" => Method::OPTIONS,
                _ => {
                    return (
                        axum::http::StatusCode::METHOD_NOT_ALLOWED,
                        "method not allowed",
                    )
                        .into_response();
                }
            };
            let path = uri.path().to_string();
            let query_string = uri.query().unwrap_or("").to_string();
            let headers: Vec<(String, String)> = parts
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            // Extract path params from axum's already-parsed route match
            let params = match RawPathParams::from_request_parts(&mut parts, &()).await {
                Ok(raw) => raw
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<HashMap<_, _>>(),
                _ => HashMap::new(),
            };

            // Reconstruct request for body consumption
            let req = Request::from_parts(parts, body);
            let body = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
                Ok(b) => b.to_vec(),
                Err(_) => {
                    return (axum::http::StatusCode::BAD_REQUEST, "Failed to read body")
                        .into_response();
                }
            };

            if tx
                .send(IncomingRequest::Http(HttpRequest {
                    handler_index,
                    method,
                    path,
                    query_string,
                    params,
                    headers,
                    body,
                    response_tx: res_tx,
                }))
                .is_err()
            {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Server is shutting down",
                )
                    .into_response();
            }

            match res_rx.await {
                Ok(msg) => match msg {
                    HttpResponseMessage::Normal {
                        status,
                        headers,
                        body,
                    } => {
                        let status = axum::http::StatusCode::from_u16(status)
                            .unwrap_or(axum::http::StatusCode::OK);
                        let mut response = AxumResponse::new(Body::from(body.unwrap_or_default()));
                        *response.status_mut() = status;
                        for (k, v) in &headers {
                            if let (Ok(name), Ok(val)) = (
                                axum::http::HeaderName::from_bytes(k.as_bytes()),
                                axum::http::HeaderValue::from_str(v),
                            ) {
                                response.headers_mut().insert(name, val);
                            }
                        }
                        response
                    }
                    HttpResponseMessage::Streaming {
                        status,
                        headers,
                        receiver,
                    } => {
                        use futures_util::StreamExt;
                        // Map each frame to an SSE `data:` event.
                        let stream = ReceiverStream::new(receiver)
                            .map(|data| Ok::<_, std::io::Error>(Event::default().data(data)));
                        let mut response = Sse::new(stream)
                            .keep_alive(KeepAlive::default())
                            .into_response();
                        if let Ok(s) = axum::http::StatusCode::from_u16(status) {
                            *response.status_mut() = s;
                        }
                        for (k, v) in &headers {
                            if let (Ok(name), Ok(val)) = (
                                axum::http::HeaderName::from_bytes(k.as_bytes()),
                                axum::http::HeaderValue::from_str(v),
                            ) {
                                response.headers_mut().insert(name, val);
                            }
                        }
                        response
                    }
                },
                Err(_) => (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                )
                    .into_response(),
            }
        }
    };

    // Wrap with method-specific router so multiple routes at the same path
    // (e.g. GET and PUT /api/users/:id) can coexist without fallback conflicts.
    let handler = match method {
        Method::GET => get(handler_fn),
        Method::POST => post(handler_fn),
        Method::PUT => put(handler_fn),
        Method::PATCH => patch(handler_fn),
        Method::DELETE => delete(handler_fn),
        Method::HEAD => head(handler_fn),
        Method::OPTIONS => options(handler_fn),
    };
    (axum_path, handler)
}

/// Global counter assigning unique ids to WebSocket connections.
static NEXT_WS_ID: AtomicU64 = AtomicU64::new(1);

/// Create a route that upgrades an HTTP connection to a WebSocket and bridges
/// it to the JS poller. The bridge task (`bridge_ws`) runs on tokio; inbound
/// client messages and the close event are reported back via
/// `IncomingRequest::WebSocketData`, and the freshly-upgraded connection
/// is announced via `IncomingRequest::WebSocketOpen` (so the JS-side
/// `WebSocketStream` is created and the user's `ws()` handler runs).
fn delegate_ws_route(
    path: String,
    handler_index: usize,
    request_tx: channel::Sender<IncomingRequest>,
) -> (String, MethodRouter<()>) {
    let axum_path = express_to_axum_path(&path);
    let tx = request_tx;

    let handler_fn = move |ws: WebSocketUpgrade, uri: axum::extract::OriginalUri| {
        let tx = tx.clone();
        async move {
            let query_string = uri.0.query().unwrap_or("").to_string();
            let id = NEXT_WS_ID.fetch_add(1, Ordering::Relaxed);
            let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAP);
            let (control_tx, control_rx) = mpsc::channel::<WsControl>(8);
            let event_tx = tx.clone();

            if tx
                .send(IncomingRequest::WebSocketOpen {
                    handler_index,
                    id,
                    out_tx,
                    control_tx,
                    query: query_string,
                    params: HashMap::new(),
                })
                .is_err()
            {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Server is shutting down",
                )
                    .into_response();
            }

            // Hand the socket to the bridge task; it owns out_rx + control_rx
            // and forwards client traffic to the JS poller via event_tx.
            ws.on_upgrade(move |socket| bridge_ws(socket, out_rx, control_rx, event_tx, id))
        }
    };

    (axum_path, get(handler_fn))
}

fn js_handler_then(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_reject = cc.magic() == 1;

    if is_reject {
        let reason = cc.arg(0);
        if !reason.is_undefined()
            && let Ok(s) = reason.to_string(ctx)
        {
            eprintln!("{}", s.content().encode_utf8(false));
        }
    }

    let res_val = cc.data(0);
    let p = match get_res_payload(ctx, &res_val) {
        Ok(p) => p,
        Err(_) => return Ok(JSValue::UNDEFINED),
    };

    if !p.ended {
        p.status = 204;
        p.send_response();
    }

    Ok(JSValue::UNDEFINED)
}

fn js_listen(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // Check if already running
    if get_server_payload(ctx, &this)?.shutdown_tx.is_some() {
        ctx.throw_type_error("http server already running");
        return Err(-1);
    }

    // Parse options
    let mut port: u16 = 8080;
    let mut host: String = "0.0.0.0".into();

    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
            if let Ok(v) = opts.get_property_str(ctx, "port")
                && let Ok(n) = v.to_number(ctx)
                && let IntOrFloat::Int64(n) = n
            {
                port = n as u16;
            }

            if let Ok(v) = opts.get_property_str(ctx, "host")
                && let Ok(s) = v.to_string(ctx)
            {
                host = s.content().encode_utf8(false).to_string();
            }
        } else {
            ctx.throw_type_error_not_an_object();
            return Err(-1);
        }
    }

    // Build router, channel, and handler tables in one scope
    let (
        router,
        js_rx_opt,
        http_handler_table,
        mcp_handler_tables,
        mcp_update_rxs,
        ws_handler_table,
    ) = {
        let data = get_server_payload_mut(ctx, &mut this)?;
        data.frozen = true;

        // Base router with optional OpenAI engine routes
        let mut router: Router = Router::new();
        #[cfg(feature = "llama_cpp")]
        if let Some((ref eng, ref mount_path)) = data.openai {
            let engine_router = engine_openai_router(eng.clone());
            router = router.nest(mount_path, engine_router);
        }

        // --- Static file mounts: served directly by axum, no poller needed ---
        for (mount, dir) in &data.static_dirs {
            let mount = if mount.ends_with('/') {
                mount.clone()
            } else {
                format!("{mount}/")
            };
            if mount == "/" {
                // axum 0.8 forbids nesting at the root; a root mount is the
                // catch-all for anything not matched by a JS route, so it is a
                // `fallback_service`. JS `route()`s always take precedence.
                router = router
                    .fallback_service(ServeDir::new(dir).append_index_html_on_directories(true));
            } else {
                router = router.nest_service(
                    &mount,
                    ServeDir::new(dir).append_index_html_on_directories(true),
                );
            }
        }

        let has_js = !data.js_routes.is_empty()
            || !data.mcp_endpoints.is_empty()
            || !data.ws_routes.is_empty();
        if !has_js {
            (router, None, Vec::new(), Vec::new(), Vec::new(), Vec::new())
        } else {
            let (js_tx, js_rx) = channel::unbounded::<IncomingRequest>();

            // --- HTTP routes ---
            let http_table: Vec<Object> = data.js_routes.iter().map(|(_, _, h)| *h).collect();
            for (i, (method, path, _)) in data.js_routes.iter().enumerate() {
                let (axum_path, handler) =
                    delegate_js_route(*method, path.clone(), i, js_tx.clone());
                if let Err(e) = validate_axum_path(&axum_path) {
                    ctx.throw_type_error(&e);
                    return Err(-1);
                }
                router = router.route(&axum_path, handler);
            }

            // --- WS routes ---
            let ws_table: Vec<Object> = data.ws_routes.iter().map(|(_, h)| *h).collect();
            for (i, (path, _)) in data.ws_routes.iter().enumerate() {
                let (axum_path, handler) = delegate_ws_route(path.clone(), i, js_tx.clone());
                if let Err(e) = validate_axum_path(&axum_path) {
                    ctx.throw_type_error(&e);
                    return Err(-1);
                }
                router = router.route(&axum_path, handler);
            }

            // --- MCP routes: rmcp-based, protocol handled by rmcp crate ---
            let mut mcp_tables: Vec<Vec<Object>> = Vec::new();
            let mut mcp_update_rxs: Vec<channel::Receiver<Vec<Object>>> = Vec::new();
            for (i, obj) in data.mcp_endpoints.iter_mut().enumerate() {
                // Read the payload (immutable) to build the route + handler table.
                let p = obj
                    .as_native_object::<JSHttpMcp>()
                    .and_then(|o| o.payload())
                    .ok_or_else(|| {
                        ctx.throw_type_error("invalid HttpMcp endpoint");
                        -1
                    })?;

                // Build axum route via rmcp's StreamableHttpService
                let mcp_axum_router = build_axum_mcp_route(p, js_tx.clone(), i);
                router = router.merge(mcp_axum_router);

                // JS handler table for poller-side dispatch
                mcp_tables.push(p.js_handlers.borrow().clone());

                // Create update channel for hot-reload of tools
                let (utx, urx) = channel::unbounded::<Vec<Object>>();

                // Backfill the update sender into the payload (mcp-service.md §6).
                if let Some(h) = obj.as_native_object_mut::<JSHttpMcp>() {
                    if let Some(p_mut) = h.payload_mut() {
                        if let McpTransport::Http { update_tx, .. } =
                            &mut *p_mut.transport.borrow_mut()
                        {
                            *update_tx = Some(utx);
                        }
                    }
                }

                mcp_update_rxs.push(urx);
            }

            (
                router,
                Some(js_rx),
                http_table,
                mcp_tables,
                mcp_update_rxs,
                ws_table,
            )
        }
    };

    // Spawn the unified axum service
    let handle = match spawn_http_server(router, host, port) {
        Ok(h) => h,
        Err(e) => {
            ctx.throw_internal_error(&e);
            return Err(-1);
        }
    };

    // Store handle — after spawn, is_running is true
    {
        let p = get_server_payload_mut(ctx, &mut this)?;
        p.shutdown_tx = Some(handle.shutdown_tx);
        p.is_running = handle.is_running.clone();
    }

    // Clone is_running for the exit promise before handle.is_running gets moved
    let running_flag = handle.is_running.clone();

    // Keep a handle to the HttpServer instance for the poller (WS dispatch needs
    // to reach stream_registry / topic_registry). Wrapped in RefCell because the
    // poller closure is `Fn` (shared) but mutates the payload through it.
    let server_ref = RefCell::new(this.clone());

    // Start unified server job on JS thread (handles HTTP, MCP, and WS events)
    if let Some(js_rx) = js_rx_opt {
        js_start_server_job(
            ctx,
            js_rx,
            http_handler_table,
            mcp_handler_tables,
            mcp_update_rxs,
            ws_handler_table,
            server_ref,
            handle.is_running,
        );
    }

    // Create exit promise — resolves when the server fully stops
    let (mut exit_promise, exit_tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;

    // Spawn tokio task that polls is_running and resolves the exit promise
    tokio::spawn(async move {
        while running_flag.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = PollSender::from(exit_tx).send_async(Ok(())).await;
    });

    // Create handle object with exit (readonly Promise) and shutdown()
    let mut handle_obj = ctx.create_base_object().map_err(|_| {
        ctx.throw_internal_error("failed to create handle object");
        -1
    })?;

    // Define a hidden reference to the HttpServer on the exit promise.
    // This keeps the server alive as long as the promise is referenced
    // (e.g. after startServer returns handle.exit and handle goes out of scope).
    exit_promise.define_property_value_str(
        ctx,
        "_server",
        this.into(),
        0, // hidden: not configurable, not writable, not enumerable
    )?;

    // prop: exit — readonly (no WRITABLE flag)
    handle_obj.define_property_value_str(
        ctx,
        "exit",
        exit_promise.into(),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    // method: shutdown — function that sends stop signal
    let shutdown_fn = ctx.create_native_function_with_data(
        Callback::Func(js_handle_shutdown),
        "shutdown",
        0,
        0,
        [this].into(),
    )?;
    handle_obj.define_property_value_str(
        ctx,
        "shutdown",
        shutdown_fn.into(),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    Ok(handle_obj.into())
}

/// Start the unified server job via queue_job — polls crossbeam channel on JS thread.
/// Handles HTTP, MCP tool call, and WebSocket stream requests through the same channel.
fn js_start_server_job(
    ctx: &mut ScopedContext,
    rx: channel::Receiver<IncomingRequest>,
    http_handler_table: Vec<Object>,
    mcp_handler_tables: Vec<Vec<Object>>,
    mcp_update_rxs: Vec<channel::Receiver<Vec<Object>>>,
    ws_handler_table: Vec<Object>,
    server_ref: RefCell<JSValue>,
    is_running: Arc<AtomicBool>,
) {
    let is_running2 = is_running.clone();
    // Wrap MCP tables in RefCell for hot-reload support
    let mcp_tables = RefCell::new(mcp_handler_tables);
    crate::helpers::start_channel_poller(
        ctx,
        "http_mcp",
        rx,
        move |incoming: IncomingRequest, cx| {
            // Check for tool table updates before processing
            for (i, urx) in mcp_update_rxs.iter().enumerate() {
                if let Ok(new_table) = urx.try_recv() {
                    mcp_tables.borrow_mut()[i] = new_table;
                }
            }
            match incoming {
                IncomingRequest::Http(req) => {
                    dispatch_http_request(cx, req, &http_handler_table);
                }
                IncomingRequest::McpToolCall(req) => {
                    handle_mcp_tool_call(cx, req, &mcp_tables.borrow());
                }
                IncomingRequest::WebSocketOpen {
                    handler_index,
                    id,
                    out_tx,
                    control_tx,
                    query,
                    params,
                } => {
                    let mut sv = server_ref.borrow_mut();
                    dispatch_ws_open(
                        cx,
                        handler_index,
                        id,
                        out_tx,
                        query,
                        params,
                        control_tx,
                        &ws_handler_table,
                        &mut *sv,
                    );
                }
                IncomingRequest::WebSocketData { id, event } => {
                    let mut sv = server_ref.borrow_mut();
                    dispatch_ws_data(cx, id, event, &mut *sv);
                }
            }
            true // keep polling
        },
        move || is_running2.load(Ordering::Acquire),
        || {},
    );
}

/// Dispatch an HTTP request to the appropriate JS handler.
fn dispatch_http_request(cx: &mut ScopedContext, incoming: HttpRequest, handler_table: &[Object]) {
    let handler = &handler_table[incoming.handler_index];
    let query = parse_query_string(&incoming.query_string);

    let req_val: JSValue = match JSHttpRequest::new_instance(
        cx,
        JSValue::UNDEFINED,
        HttpRequestPayload {
            method: incoming.method,
            path: incoming.path.clone(),
            params: incoming.params.clone(),
            query,
            headers: incoming.headers.iter().cloned().collect(),
            body: Some(incoming.body),
        },
    ) {
        Ok(v) => v.into(),
        Err(_) => return,
    };

    let res_val: JSValue = match JSHttpResponse::new_instance(
        cx,
        JSValue::UNDEFINED,
        HttpResponsePayload {
            status: 200,
            headers: Vec::new(),
            body: None,
            ended: false,
            switched: false,
            response_tx: Some(incoming.response_tx),
        },
    ) {
        Ok(v) => v.into(),
        Err(_) => return,
    };

    // Call JS handler
    match handler.call(cx, JSValue::UNDEFINED, &[req_val, res_val]) {
        Ok(val) => {
            if let Ok(mut promise) = Promise::try_from(val) {
                // Async handler — attach then/catch
                let ok_fn = match cx.create_native_function_with_data(
                    Callback::Func(js_handler_then),
                    "",
                    0,
                    0,
                    [res_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => return,
                };
                let fail_fn = match cx.create_native_function_with_data(
                    Callback::Func(js_handler_then),
                    "",
                    1,
                    1,
                    [res_val].into(),
                ) {
                    Ok(f) => JSValue::from(f),
                    Err(_) => return,
                };

                if promise.then(cx, ok_fn, fail_fn).is_err() {
                    if let Ok(p) = get_res_payload(cx, &val) {
                        p.status = 500;
                        p.body = Some(String::from("Internal Server Error"));
                        p.send_response();
                    }
                }
            } else {
                // Sync handler
                if let Ok(p) = get_res_payload(cx, &res_val)
                    && !p.ended
                {
                    p.status = 204;
                    p.send_response();
                }
            }
        }
        Err(_) => {
            let exc = cx.take_exception();
            let msg = exc.to_text(cx);
            if !msg.message.is_empty() {
                eprintln!("{}", msg.message);
            }
            if let Ok(p) = get_res_payload(cx, &res_val)
                && !p.ended
            {
                p.status = 500;
                p.body = Some(String::from("Internal Server Error"));
                p.send_response();
            }
        }
    }
}

fn js_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    let (shutdown_tx, is_running) = {
        let p = get_server_payload_mut(ctx, &mut this)?;
        if p.shutdown_tx.is_none() {
            return Ok(JSValue::UNDEFINED);
        }

        let tx = p.shutdown_tx.take();
        let r = p.is_running.clone();
        p.frozen = false;
        (tx, r)
    };

    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }

    // Create promise that resolves when server fully stops
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;

    tokio::spawn(async move {
        while is_running.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = PollSender::from(tx).send_async(Ok(())).await;
    });

    Ok(promise.into())
}

/// Handle's shutdown — sends stop signal only, returns immediately.
/// The caller should await handle.exit to wait for the server to fully stop.
fn js_handle_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut server_val = cc.data(0);
    let shutdown_tx = {
        let p = get_server_payload_mut(ctx, &mut server_val)?;
        if p.shutdown_tx.is_none() {
            return Ok(JSValue::UNDEFINED);
        }
        let tx = p.shutdown_tx.take();
        p.frozen = false;
        tx
    };
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }
    Ok(JSValue::UNDEFINED)
}

fn js_is_running(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_server_payload(ctx, &this)?;
    Ok(JSValue::from(data.shutdown_tx.is_some()))
}

fn parse_query_string(qs: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if qs.is_empty() {
        return map;
    }
    for pair in qs.split('&') {
        if let Some(pos) = pair.find('=') {
            map.insert(url_decode(&pair[..pos]), url_decode(&pair[pos + 1..]));
        } else {
            map.insert(url_decode(pair), String::new());
        }
    }
    map
}

fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().and_then(hex_val);
            let lo = chars.next().and_then(hex_val);
            if let (Some(h), Some(l)) = (hi, lo) {
                result.push((h << 4 | l) as char);
            } else {
                result.push('%');
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// HttpServer.ws(path, handler) — register a WebSocket route.
fn js_route_ws(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let path = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let handler = cc.arg(1).into_callable(ctx)?;
    let p = get_server_payload_mut(ctx, &mut this)?;
    if p.frozen {
        ctx.throw_type_error("cannot add ws route while server is running");
        return Err(-1);
    }
    p.ws_routes.push((path, handler));
    Ok(this)
}

/// HttpServer.staticFiles(mountPath, dir) — serve a directory of static files.
fn js_static_files(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let mount = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    if !mount.starts_with('/') {
        ctx.throw_type_error("mountPath must start with '/'");
        return Err(-1);
    }
    let dir = cc
        .arg(1)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let p = get_server_payload_mut(ctx, &mut this)?;
    if p.frozen {
        ctx.throw_type_error("cannot add static dir while server is running");
        return Err(-1);
    }
    p.static_dirs.push((mount, dir));
    Ok(this)
}

/// HttpServer.publish(topic, data) — broadcast `data` to all WS streams that
/// subscribed to `topic` (Bun server.publish semantics).
fn js_publish(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let topic = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let msg = cc
        .arg(1)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    let mut this = cc.this();
    let p = get_server_payload_mut(ctx, &mut this)?;

    // Snapshot the subscriber ids for this topic.
    let ids: Vec<u64> = match p.topic_registry.borrow().get(&topic) {
        Some(set) => set.iter().copied().collect(),
        None => return Ok(JSValue::UNDEFINED),
    };

    for id in ids {
        if let Some(mut obj) = p.stream_registry.borrow().get(&id).cloned() {
            if let Some(ws) = obj
                .as_native_object_mut::<JSWebSocketStream>()
                .and_then(|o| o.payload_mut())
            {
                if let Some(tx) = &ws.sink.tx {
                    let _ = tx.try_send(msg.clone());
                }
            }
        }
    }

    Ok(JSValue::UNDEFINED)
}

const SERVER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("get", 2, js_route_add, 0),
    FieldDef::FUNC("post", 2, js_route_add, 1),
    FieldDef::FUNC("put", 2, js_route_add, 2),
    FieldDef::FUNC("patch", 2, js_route_add, 3),
    FieldDef::FUNC("delete", 2, js_route_add, 4),
    FieldDef::FUNC("head", 2, js_route_add, 5),
    FieldDef::FUNC("options", 2, js_route_add, 6),
    FieldDef::FUNC("mcp", 2, js_mcp, 0),
    FieldDef::FUNC("removeRoute", 2, js_remove_route, 0),
    FieldDef::FUNC("ws", 2, js_route_ws, 0),
    FieldDef::FUNC("staticFiles", 2, js_static_files, 0),
    FieldDef::FUNC("publish", 2, js_publish, 0),
    #[cfg(feature = "llama_cpp")]
    FieldDef::FUNC("openai", 2, js_openai, 0),
    FieldDef::GETSET("isRunning", Some(js_is_running), None, 0),
    FieldDef::FUNC("listen", 1, js_listen, 0),
    FieldDef::FUNC("shutdown", 0, js_shutdown, 0),
];

def_native_object!(
    "HttpServer",
    JSHttpServer,
    HttpServerPayload,
    SERVER_PROTO,
    Some(ClassCtorDef::new(js_constructor, 0, false)),
    on_payload_gc: HttpServerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_server(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpServer::register(rt)
}
