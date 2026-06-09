// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod request;
mod response;

pub(crate) use {request::JSHttpRequest, response::JSHttpResponse};

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::Method,
    response::{IntoResponse, Response as AxumResponse},
    routing::any,
};
use crossbeam::channel;
use linkme::distributed_slice;
use matchit::Router as MatchRouter;
use tokio::sync::oneshot;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue,
    NativeObject, Object, PollSender, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use {
    request::HttpRequestPayload,
    response::{HttpResponseMessage, HttpResponsePayload, get_res_payload},
};

struct IncomingRequest {
    method: String,
    path: String,
    query_string: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    response_tx: oneshot::Sender<HttpResponseMessage>,
}

#[derive(Clone)]
struct AxumState {
    request_tx: channel::Sender<IncomingRequest>,
}

pub(crate) struct HttpServerPayload {
    routes: Vec<(Method, String, Object)>,
    request_tx: Option<channel::Sender<IncomingRequest>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    is_running: Arc<AtomicBool>,
}

impl HttpServerPayload {
    fn add_route(&mut self, method: Method, path: String, handler: Object) {
        self.routes.push((method, path, handler));
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for (_, _, handler) in &self.routes {
            handler.trace(gcx);
        }
    }
}

/// Convert Express-style `:param` to matchit-style `{param}`.
fn normalize_route(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' {
            let mut name = String::new();
            for ch in chars.by_ref() {
                if ch.is_alphanumeric() || ch == '_' {
                    name.push(ch);
                } else {
                    out.push_str(&format!("{{{}}}", name));
                    out.push(ch);
                    break;
                }
            }
            if !name.is_empty() {
                out.push_str(&format!("{{{}}}", name));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn get_server_payload<'a>(
    ctx: &'a mut ScopedContext,
    val: &JSValue,
) -> Result<&'a mut HttpServerPayload, i32> {
    // let payload = val
    //     .as_native_object::<JSHttpServer>()
    //     .ok_or_else(|| ctx.throw_internal_error("HttpServer expected"))?;

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
    /// Set before calling `promise.then()`, read in the success/fail callbacks.
    static ASYNC_RES: RefCell<Option<JSValue>> = const { RefCell::new(None) };
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    JSHttpServer::new_instance(
        ctx,
        cc.this(),
        Box::new(HttpServerPayload {
            routes: Vec::new(),
            request_tx: None,
            shutdown_tx: None,
            is_running: Arc::new(AtomicBool::new(false)),
        }),
    )
    .map(|v| v.into())
}

macro_rules! route_method {
    ($name:ident, $method:expr) => {
        fn $name(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
            let this = cc.this();
            // Extract args before borrowing payload to avoid ctx double-borrow
            let path_str = cc.arg(0).to_string(ctx)?;
            let path = path_str.content().encode_utf8(false).to_string();
            let handler = cc.arg(1).into_callable(ctx)?;
            let data = get_server_payload(ctx, &this)?;
            data.add_route($method, path, handler);
            Ok(this)
        }
    };
}

route_method!(js_route_get, Method::GET);
route_method!(js_route_post, Method::POST);
route_method!(js_route_put, Method::PUT);
route_method!(js_route_patch, Method::PATCH);
route_method!(js_route_delete, Method::DELETE);
route_method!(js_route_head, Method::HEAD);
route_method!(js_route_options, Method::OPTIONS);

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

    // Check if already running — shutdown_tx.is_some() means server is active
    if get_server_payload(ctx, &this)?.shutdown_tx.is_some() {
        ctx.throw_type_error("http server already running");
        return Err(-1);
    }

    // Parse options before borrowing payload
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

    // Now extract routes from payload (separate borrow scope)
    let routes = {
        let data = get_server_payload(ctx, &this)?;
        data.routes.clone()
    };

    // Build per-method matchit routers (convert `:param` to `{param}`)
    let mut method_routers: HashMap<String, MatchRouter<usize>> = HashMap::new();
    for (i, (method, path, _)) in routes.iter().enumerate() {
        let entry = method_routers.entry(method.to_string()).or_default();
        let _ = entry.insert(normalize_route(path), i);
    }

    // Create channels
    let (request_tx, request_rx) = channel::unbounded::<IncomingRequest>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // Store channels and mark running
    let is_running: Arc<AtomicBool> = {
        let data = get_server_payload(ctx, &this)?;
        data.request_tx = Some(request_tx.clone());
        data.shutdown_tx = Some(shutdown_tx);
        data.is_running.clone()
    };
    is_running.store(true, Ordering::Release);

    // Start poller that processes incoming requests on the JS thread
    let is_running2 = is_running.clone();
    crate::helpers::start_channel_poller(
        ctx,
        request_rx,
        move |incoming: IncomingRequest, cx| {
            let handler_idx = method_routers
                .get(&incoming.method)
                .and_then(|r| r.at(&incoming.path).ok())
                .map(|m| *m.value);

            if let Some(idx) = handler_idx {
                let (_, _, handler) = &routes[idx];
                let query = parse_query_string(&incoming.query_string);

                let req_val: JSValue = match JSHttpRequest::new_instance(
                    cx,
                    JSValue::UNDEFINED,
                    Box::new(HttpRequestPayload {
                        method: incoming.method.clone(),
                        path: incoming.path.clone(),
                        params: {
                            let mut map = HashMap::new();
                            if let Some(m) = method_routers
                                .get(&incoming.method)
                                .and_then(|r| r.at(&incoming.path).ok())
                            {
                                for (k, v) in m.params.iter() {
                                    map.insert(k.to_string(), v.to_string());
                                }
                            }
                            map
                        },
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

                // call handler
                match handler.call(cx, JSValue::UNDEFINED, &[req_val, res_val]) {
                    Ok(val) => {
                        if let Ok(mut promise) = Promise::try_from(val) {
                            // handler is async function
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
                                1, // reject
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
            } else {
                let _ = incoming.response_tx.send(HttpResponseMessage {
                    status: 404,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: Some(r#"{"error":"not found"}"#.into()),
                });
            }

            true // keep polling
        },
        move || is_running2.load(Ordering::Acquire),
        || {},
    );

    // Spawn axum server
    tokio::spawn(async move {
        let axum_state = AxumState {
            request_tx: request_tx.clone(),
        };
        let axum_state_for_ws = axum_state.clone();

        let app = Router::new()
            .route(
                "/{*path}",
                any(move |req: Request| {
                    let st = axum_state.clone();
                    async move { axum_catch_all(st, req).await }
                }),
            )
            .with_state(axum_state_for_ws);

        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("HttpServer failed to bind {host}:{port}: {e}");
                return;
            }
        };

        log::info!("HttpServer listening on {host}:{port}");

        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async { drop(shutdown_rx.await) })
            .await;

        // Mark not running after server fully stops
        is_running.store(false, Ordering::Release);
    });

    Ok(JSValue::UNDEFINED)
}

async fn axum_catch_all(axum_state: AxumState, req: Request) -> AxumResponse {
    let method = req.method().to_string();
    let uri = req.uri().clone();
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
            return (axum::http::StatusCode::BAD_REQUEST, "Failed to read body").into_response();
        }
    };

    let (res_tx, res_rx) = oneshot::channel();

    if axum_state
        .request_tx
        .send(IncomingRequest {
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
            let status =
                axum::http::StatusCode::from_u16(msg.status).unwrap_or(axum::http::StatusCode::OK);
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

fn js_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    let (shutdown_tx, is_running) = {
        let data = get_server_payload(ctx, &this)?;
        // shutdown_tx.is_some() means server is running
        if data.shutdown_tx.is_none() {
            return Ok(JSValue::UNDEFINED);
        }
        let tx = data.shutdown_tx.take();
        let r = data.is_running.clone();
        (tx, r)
    };

    // Send shutdown signal
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(());
    }

    // Create promise that resolves when server fully stops
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), ()>>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;

    tokio::spawn(async move {
        // Spin until is_running goes false (signals axum server fully stopped)
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
    FieldDef::FUNC("get", 2, js_route_get, 0),
    FieldDef::FUNC("post", 2, js_route_post, 0),
    FieldDef::FUNC("put", 2, js_route_put, 0),
    FieldDef::FUNC("patch", 2, js_route_patch, 0),
    FieldDef::FUNC("delete", 2, js_route_delete, 0),
    FieldDef::FUNC("head", 2, js_route_head, 0),
    FieldDef::FUNC("options", 2, js_route_options, 0),
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
