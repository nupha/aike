// JS binding for HttpServer.
//
// HttpServer is a JS-side server instance with full lifecycle:
// create → register routes → listen → shutdown.
// Rust-side state is carried by HttpServerPayload.
// JS routes are delegated to axum via crossbeam channel + handler_index lookup.

use std::{
    cell::Cell,
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::Request,
    response::{IntoResponse, Response as AxumResponse},
    routing::{MethodRouter, delete, get, head, options, patch, post, put},
};
use crossbeam::channel;
use linkme::distributed_slice;
use tokio::sync::oneshot;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, Object,
    PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    llm::{
        JSEngine,
        engine::{Engine, serve::engine_openai_router},
    },
};

use super::{
    Method,
    request::{HttpRequestPayload, JSHttpRequest},
    response::{HttpResponseMessage, HttpResponsePayload, JSHttpResponse, get_res_payload},
    server::spawn_http_server,
};

/// Request message sent from axum delegate fn to the JS HTTP Server Job.
struct IncomingRequest {
    handler_index: usize,
    method: Method,
    path: String,
    query_string: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    response_tx: oneshot::Sender<HttpResponseMessage>,
}

pub(super) struct HttpServerPayload {
    openai: Option<(Arc<Mutex<Engine>>, String)>,
    js_routes: Vec<(Method, String, Object)>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    is_running: Arc<AtomicBool>,
    frozen: bool,
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

fn get_server_payload_mut<'a>(
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
            openai: None,
            js_routes: Vec::new(),
            shutdown_tx: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frozen: false,
        },
    )
    .map(|v| v.into())
}

/// HttpServer.openai(engine, mountPath) — mount OpenAI-compatible routes.
///
/// `engine` must be an Engine instance (ownership is transferred).
/// `mountPath` must start with '/', e.g. "/v1".
/// Calling after listen() raises an error.
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
            for ch in chars.by_ref() {
                if ch.is_alphanumeric() || ch == '_' {
                    param.push(ch);
                } else {
                    result.push('{');
                    result.push_str(&param);
                    result.push('}');
                    result.push(ch);
                    break;
                }
            }
            if !param.is_empty() {
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
    let path = express_to_axum_path(&path);
    let tx = request_tx;

    // Create the handler closure once, then wrap with method-specific router
    let handler_fn = move |req: Request| {
        let tx = tx.clone();
        async move {
            let (res_tx, res_rx) = oneshot::channel();
            let uri = req.uri().clone();
            let method = match req.method().as_str() {
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
            let headers: Vec<(String, String)> = req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            let body = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
                Ok(b) => b.to_vec(),
                Err(_) => {
                    return (axum::http::StatusCode::BAD_REQUEST, "Failed to read body")
                        .into_response();
                }
            };

            if tx
                .send(IncomingRequest {
                    handler_index,
                    method,
                    path,
                    query_string,
                    headers,
                    body,
                    response_tx: res_tx,
                })
                .is_err()
            {
                return (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Server is shutting down",
                )
                    .into_response();
            }

            match res_rx.await {
                Ok(msg) => {
                    let status = axum::http::StatusCode::from_u16(msg.status)
                        .unwrap_or(axum::http::StatusCode::OK);
                    let mut response = AxumResponse::new(Body::from(msg.body.unwrap_or_default()));
                    *response.status_mut() = status;
                    for (k, v) in &msg.headers {
                        if let (Ok(name), Ok(val)) = (
                            axum::http::HeaderName::from_bytes(k.as_bytes()),
                            axum::http::HeaderValue::from_str(v),
                        ) {
                            response.headers_mut().insert(name, val);
                        }
                    }
                    response
                }
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
    (path, handler)
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
    let p = get_res_payload(ctx, &res_val).unwrap();

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

    // Extract data from payload (separate borrow scope)
    let (js_routes, openai) = {
        let data = get_server_payload_mut(ctx, &mut this)?;
        data.frozen = true;
        (data.js_routes.clone(), data.openai.clone())
    };

    // Build the base router. If openai engine + mountPath provided, nest
    // engine routes under the mount path. JS routes are added at the top level.
    let mut router: Router = Router::new();
    if let Some((ref eng, ref mount_path)) = openai {
        let engine_router = engine_openai_router(eng.clone());
        router = router.nest(mount_path, engine_router);
    }

    let mut js_rx_opt: Option<channel::Receiver<IncomingRequest>> = None;
    let mut handler_table_opt: Option<Vec<Object>> = None;

    if !js_routes.is_empty() {
        let (js_tx, js_rx) = channel::unbounded::<IncomingRequest>();

        // Build handler lookup table (Object stays on JS thread)
        let handler_table: Vec<Object> = js_routes.iter().map(|(_, _, h)| *h).collect();

        // Delegate each JS route and add to router
        for (i, (method, path, _)) in js_routes.iter().enumerate() {
            let (path, handler) = delegate_js_route(*method, path.clone(), i, js_tx.clone());
            router = router.route(&path, handler);
        }

        js_rx_opt = Some(js_rx);
        handler_table_opt = Some(handler_table);
    }

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

    // Start HTTP Server Job on JS thread with the CORRECT is_running flag
    if let (Some(js_rx), Some(handler_table)) = (js_rx_opt, handler_table_opt) {
        js_start_http_server_job(ctx, js_rx, handler_table, handle.is_running);
    }

    Ok(JSValue::UNDEFINED)
}

/// Start the HTTP Server Job via queue_job — polls crossbeam channel on JS thread.
fn js_start_http_server_job(
    ctx: &mut ScopedContext,
    rx: channel::Receiver<IncomingRequest>,
    handler_table: Vec<Object>,
    is_running: Arc<AtomicBool>,
) {
    let is_running2 = is_running.clone();
    crate::helpers::start_channel_poller(
        ctx,
        rx,
        move |incoming: IncomingRequest, cx| {
            // Lookup handler by index — no route matching needed
            let handler = &handler_table[incoming.handler_index];
            let query = parse_query_string(&incoming.query_string);

            let req_val: JSValue = match JSHttpRequest::new_instance(
                cx,
                JSValue::UNDEFINED,
                HttpRequestPayload {
                    method: incoming.method,
                    path: incoming.path.clone(),
                    params: HashMap::new(),
                    query,
                    headers: incoming.headers.iter().cloned().collect(),
                    body: Some(incoming.body),
                },
            ) {
                Ok(v) => v.into(),
                Err(_) => return true,
            };

            let res_val: JSValue = match JSHttpResponse::new_instance(
                cx,
                JSValue::UNDEFINED,
                HttpResponsePayload {
                    status: 200,
                    headers: Vec::new(),
                    body: None,
                    ended: false,
                    response_tx: Some(incoming.response_tx),
                },
            ) {
                Ok(v) => v.into(),
                Err(_) => return true,
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
                            Err(_) => return true,
                        };
                        let fail_fn = match cx.create_native_function_with_data(
                            Callback::Func(js_handler_then),
                            "",
                            1,
                            1,
                            [res_val].into(),
                        ) {
                            Ok(f) => JSValue::from(f),
                            Err(_) => return true,
                        };

                        if promise.then(cx, ok_fn, fail_fn).is_err() {
                            let p = get_res_payload(cx, &val).unwrap();
                            p.status = 500;
                            p.body = Some(String::from("Internal Server Error"));
                            p.send_response();
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

            true // keep polling
        },
        move || is_running2.load(Ordering::Acquire),
        || {},
    );
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

const SERVER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("openai", 2, js_openai, 0),
    FieldDef::FUNC("get", 2, js_route_add, 0),
    FieldDef::FUNC("post", 2, js_route_add, 1),
    FieldDef::FUNC("put", 2, js_route_add, 2),
    FieldDef::FUNC("patch", 2, js_route_add, 3),
    FieldDef::FUNC("delete", 2, js_route_add, 4),
    FieldDef::FUNC("head", 2, js_route_add, 5),
    FieldDef::FUNC("options", 2, js_route_add, 6),
    FieldDef::FUNC("removeRoute", 2, js_remove_route, 0),
    FieldDef::GETSET("isRunning", Some(js_is_running), None, 0),
    FieldDef::FUNC("listen", 1, js_listen, 0),
    FieldDef::FUNC("shutdown", 0, js_shutdown, 0),
];

def_native_object!(
    "HttpServer",
    JSHttpServer,
    HttpServerPayload,
    SERVER_PROTO,
    constructor: ClassCtorDef::new(js_constructor, 0, false),
    on_payload_gc: HttpServerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_server(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpServer::register(rt)
}
