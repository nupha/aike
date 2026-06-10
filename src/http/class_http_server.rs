// JS binding for HttpServer.
//
// HttpServer is a JS-side server instance with full lifecycle:
// create → register routes → listen → shutdown.
// Rust-side state is carried by HttpServerPayload.
// JS routes are delegated to axum via crossbeam channel + handler_index lookup.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    body::Body,
    extract::Request,
    response::{IntoResponse, Response as AxumResponse},
    routing::any,
};
use crossbeam::channel;
use linkme::distributed_slice;
use tokio::sync::oneshot;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue,
    NativeObject, Object, PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;
use crate::llm::engine::Engine;
use crate::llm::engine::serve::engine_routes;

use super::Method;
use super::request::{HttpRequestPayload, JSHttpRequest};
use super::response::{HttpResponseMessage, HttpResponsePayload, JSHttpResponse, get_res_payload};
use super::server::{RouteEntry, spawn_http_service};

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

pub(crate) struct HttpServerPayload {
    engine: Option<Arc<Mutex<Engine>>>,
    rust_routes: Vec<RouteEntry>,
    js_routes: Vec<(Method, String, Object)>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    is_running: Arc<AtomicBool>,
    frozen: bool,
}

impl HttpServerPayload {
    fn add_route(
        &mut self,
        method: Method,
        path: String,
        handler: Object,
    ) -> Result<(), &'static str> {
        if self.frozen {
            return Err("cannot add route while server is running");
        }
        self.js_routes.push((method, path, handler));
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
    ctx: &'a mut ScopedContext,
    val: &JSValue,
) -> Result<&'a mut HttpServerPayload, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSHttpServer::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_internal_error("HttpServer was disposed");
        Err(-1)
    } else {
        Ok(unsafe { &mut *opaque.cast::<HttpServerPayload>() })
    }
}

thread_local! {
    /// Temporary storage for the response object when the handler returned a Promise.
    static ASYNC_RES: RefCell<Option<JSValue>> = const { RefCell::new(None) };
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut engine: Option<Arc<Mutex<Engine>>> = None;
    let mut rust_routes = Vec::new();

    // Parse optional { engine } from first argument
    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
            if let Ok(v) = opts.get_property_str(ctx, "engine") {
                if !v.is_undefined() && !v.is_null() {
                    // Extract engine Arc<Mutex<Engine>> from JSEngine native object
                    if let Some(e) = v.as_native_object::<crate::llm::JSEngine>() {
                        if let Some(payload) = e.payload() {
                            engine = Some(payload.clone());
                        }
                    }
                }
            }
        }
    }

    // If engine is provided, pre-load Engine routes
    if let Some(ref eng) = engine {
        rust_routes = engine_routes(eng.clone());
    }

    JSHttpServer::new_instance(
        ctx,
        cc.this(),
        Box::new(HttpServerPayload {
            engine,
            rust_routes,
            js_routes: Vec::new(),
            shutdown_tx: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frozen: false,
        }),
    )
    .map(|v| v.into())
}

fn js_route_add(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
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
    let data = get_server_payload(ctx, &this)?;
    if let Err(e) = data.add_route(method, path, handler) {
        ctx.throw_type_error(e);
        return Err(-1);
    }
    Ok(this)
}

fn js_remove_route(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
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
    let data = get_server_payload(ctx, &this)?;
    match data.remove_route(method, &path) {
        Ok(removed) => Ok(JSValue::Bool(removed)),
        Err(e) => {
            ctx.throw_type_error(e);
            Err(-1)
        }
    }
}

/// Create a RouteEntry that delegates to a JS handler via channel.
///
/// The closure captures only `handler_index: usize` and `Sender` — both Send + Sync.
/// JS Object stays in the JS thread handler lookup table.
fn delegate_js_route(
    method: Method,
    path: String,
    handler_index: usize,
    request_tx: channel::Sender<IncomingRequest>,
) -> RouteEntry {
    let tx = request_tx;
    let handler = any(move |req: Request| {
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
    });
    RouteEntry {
        method,
        path,
        handler,
    }
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
    let this = cc.this();

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
    let (rust_routes, js_routes, _engine) = {
        let data = get_server_payload(ctx, &this)?;
        data.frozen = true;
        (
            std::mem::take(&mut data.rust_routes),
            data.js_routes.clone(),
            data.engine.clone(),
        )
    };

    // Merge routes: Rust (Engine) + JS delegate
    let mut all_routes = rust_routes;

    if !js_routes.is_empty() {
        let (js_tx, js_rx) = channel::unbounded::<IncomingRequest>();

        // Build handler lookup table (Object stays on JS thread)
        let handler_table: Vec<Object> = js_routes.iter().map(|(_, _, h)| h.clone()).collect();

        // Delegate each JS route to a RouteEntry
        for (i, (method, path, _)) in js_routes.iter().enumerate() {
            all_routes.push(delegate_js_route(*method, path.clone(), i, js_tx.clone()));
        }

        // Start HTTP Server Job on JS thread
        let is_running_check = {
            let data = get_server_payload(ctx, &this)?;
            data.is_running.clone()
        };
        js_start_http_server_job(ctx, js_rx, handler_table, is_running_check);
    }

    // Spawn the unified axum service
    let handle = match spawn_http_service(all_routes, host, port) {
        Ok(h) => h,
        Err(e) => {
            ctx.throw_internal_error(&e);
            return Err(-1);
        }
    };

    // Store handle
    let data = get_server_payload(ctx, &this)?;
    data.shutdown_tx = Some(handle.shutdown_tx);
    data.is_running = handle.is_running;

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
                Box::new(HttpRequestPayload {
                    method: incoming.method,
                    path: incoming.path.clone(),
                    params: HashMap::new(),
                    query,
                    headers: incoming.headers.iter().cloned().collect(),
                    body: Some(incoming.body),
                }),
            ) {
                Ok(v) => v.into(),
                Err(_) => return true,
            };

            let res_val: JSValue = match JSHttpResponse::new_instance(
                cx,
                JSValue::UNDEFINED,
                Box::new(HttpResponsePayload {
                    status: 200,
                    headers: Vec::new(),
                    body: None,
                    ended: false,
                    response_tx: Some(incoming.response_tx),
                }),
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
    let this = cc.this();

    let (shutdown_tx, is_running) = {
        let data = get_server_payload(ctx, &this)?;
        if data.shutdown_tx.is_none() {
            return Ok(JSValue::UNDEFINED);
        }
        let tx = data.shutdown_tx.take();
        let r = data.is_running.clone();
        data.frozen = false;
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
    Box<HttpServerPayload>,
    SERVER_PROTO,
    constructor: ClassCtorDef::new(js_constructor, 0, true),
    on_payload_gc: HttpServerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_server(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpServer::register(rt)
}

const MOD_HTTP_ENTRIES: &[FieldDef] = &[FieldDef::CLASS(
    "HttpServer",
    JSHttpServer::class_id,
    zuqe::JS_PROP_ENUMERABLE,
)];

pub fn init_mod_http(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("http", MOD_HTTP_ENTRIES)?;
    Ok(())
}
