// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

use linkme::distributed_slice;
use zuqe::{
    ErrorKind, Exception,
    builtins::promise::{Promise, PromiseResultSlot},
    def_native_object,
    {CallContext, ClassCtorDef, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext},
};

use crate::{
    USER_CLASS_DEFS,
    http::client::{
        body::body_to_bytes,
        config::{HttpClientConfig, ProtocolMode},
        headers::{HeaderList, header_list_from_js},
        response::build_response_from_transport,
        transport::{TransportRequest, TransportResponse, perform_request},
    },
    whatwg::abort::{JSAbortSignal, signal_notify},
};

// ── Payload ────────────────────────────────────────────────────────────────

pub(crate) struct HttpClientPayload {
    /// Pre-built reqwest::Client (connection pool, TLS, proxy, …).
    client: reqwest::Client,
}

/// Sentinel magic value: read the HTTP method from `opts.method`.
const METHOD_FROM_OPTS: i16 = -1;

/// Magic constants for each HTTP method.
const M_GET: i16 = 0;
const M_POST: i16 = 1;
const M_PUT: i16 = 2;
const M_DELETE: i16 = 3;
const M_PATCH: i16 = 4;

/// Read an optional string property from a JS object.
fn read_str_opt(ctx: &mut ScopedContext, obj: Object, key: &str) -> Option<String> {
    let v = obj.get_property_str(ctx, key).ok()?;
    if v.is_undefined_or_null() {
        None
    } else {
        v.to_string(ctx)
            .ok()
            .map(|s| s.content().encode_utf8(false).to_string())
    }
}

/// Read an optional bool property.
fn read_bool_opt(ctx: &mut ScopedContext, obj: Object, key: &str) -> Option<bool> {
    let v = obj.get_property_str(ctx, key).ok()?;
    if v.is_undefined() {
        None
    } else {
        v.to_number(ctx).ok().map(|n| n.to_f64() != 0.0)
    }
}

// ── Constructor ────────────────────────────────────────────────────────────

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut cfg = HttpClientConfig::default();

    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
            // P0: Proxy
            if let Some(s) = read_str_opt(ctx, opts, "proxy") {
                if !s.is_empty() {
                    cfg.proxy = Some(s);
                }
            }
            if let Some(s) = read_str_opt(ctx, opts, "noProxy") {
                cfg.no_proxy = Some(s);
            }

            // P1: TLS
            if let Some(b) = read_bool_opt(ctx, opts, "rejectUnauthorized") {
                cfg.danger_accept_invalid_certs = b;
            }
            if let Some(s) = read_str_opt(ctx, opts, "ca") {
                cfg.root_cert_pem = Some(s);
            }
            if let Some(s) = read_str_opt(ctx, opts, "cert") {
                cfg.client_cert_pem = Some(s);
            }
            if let Some(s) = read_str_opt(ctx, opts, "key") {
                cfg.client_key_pem = Some(s);
            }

            // P2: Cookie
            if let Some(b) = read_bool_opt(ctx, opts, "cookieJar") {
                cfg.cookie_jar = b;
            }

            // P3: DNS / Protocol
            if let Some(s) = read_str_opt(ctx, opts, "protocol") {
                cfg.protocol = match s.as_str() {
                    "http1" => ProtocolMode::Http1Only,
                    "http2" => ProtocolMode::Http2PriorKnowledge,
                    _ => ProtocolMode::Auto,
                };
            }

            // resolve: TODO — expose Record<string, string> property iteration
            // (needs zuqe property enumeration API).  Phase 5.

            // P4: Connection
            if let Some(s) = read_str_opt(ctx, opts, "connectTimeout") {
                if let Ok(n) = s.parse::<f64>() {
                    if n > 0.0 {
                        cfg.connect_timeout = Duration::from_secs_f64(n);
                    }
                }
            }
        }
    }

    let client = cfg.build_http_client().map_err(|e| {
        ctx.throw_type_error(&format!("HttpClient config error: {e}"));
        -1
    })?;

    JSHttpClient::new_instance(ctx, cc.this(), HttpClientPayload { client }).map(JSValue::from)
}

// ── Error type for async result slot ──────────────────────────────────────

enum RequestError {
    Abort,
    Other(String),
}

fn throw_abort_error(cx: &mut ScopedContext) -> JSResult {
    let mut err = match cx.create_error_object(ErrorKind::Type, "The operation was aborted", false)
    {
        Some(e) => e,
        None => return Err(-1),
    };
    let name = cx.create_string_from_utf8(b"AbortError".to_vec())?;
    err.define_property_value_str(
        cx,
        "name",
        JSValue::make_string(name),
        zuqe::JS_PROP_WRITABLE | zuqe::JS_PROP_CONFIGURABLE,
    )?;
    cx.throw(Exception::Error(err.into()));
    Err(-1)
}

// ── Core request method (dispatched by magic) ──────────────────────────────

fn js_do_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = JSHttpClient::payload_of(&this).ok_or_else(|| {
        ctx.throw_internal_error("not an HttpClient instance");
        -1
    })?;
    let client = payload.client.clone();

    // Parse URL.
    let url_str = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    let magic = cc.magic() as i16;

    // Parse options.
    let mut method = String::from("GET");
    let mut req_headers = HeaderList::new();
    let mut req_body: Option<Vec<u8>> = None;
    let mut timeout = Duration::from_secs(30);
    let mut redirect = String::from("follow");
    let mut signal_val: Option<JSValue> = None;

    if cc.length() > 1 {
        if let Some(opts) = cc.arg(1).as_object() {
            // ── Method ──
            if magic == METHOD_FROM_OPTS {
                if let Some(m) = read_str_opt(ctx, opts, "method") {
                    method = m.to_uppercase();
                }
            } else {
                method = match magic {
                    M_GET => "GET",
                    M_POST => "POST",
                    M_PUT => "PUT",
                    M_DELETE => "DELETE",
                    M_PATCH => "PATCH",
                    _ => "GET",
                }
                .to_string();
            }

            // ── Headers ──
            let mut h = opts.get_property_str(ctx, "headers")?;
            if !h.is_undefined_or_null() {
                req_headers = header_list_from_js(ctx, &mut h)?;
            }

            // ── Body ──
            let b = opts.get_property_str(ctx, "body")?;
            if !b.is_undefined_or_null() {
                req_body = Some(body_to_bytes(ctx, &b)?);
            }

            // ── Timeout ──
            let t = opts.get_property_str(ctx, "timeout")?;
            if !t.is_undefined() {
                let secs = t.to_number(ctx)?.to_f64();
                if secs > 0.0 {
                    timeout = Duration::from_secs_f64(secs);
                }
            }

            // ── Redirect ──
            let r = opts.get_property_str(ctx, "redirect")?;
            if !r.is_undefined() {
                redirect = r.to_string(ctx)?.content().encode_utf8(false).to_string();
            }

            // ── Signal (AbortController / AbortSignal) ──
            let s = opts.get_property_str(ctx, "signal")?;
            if !s.is_undefined_or_null() {
                // Accept anything non-nullish as a signal. Signal-notify and
                // abort detection use the JS-level aborted property which works
                // even when the native-object pointer isn't accessible.
                signal_val = Some(s.clone());
            }
        }
    }

    // ── Abort detection (via JS property readout) ──
    let already_aborted = signal_val
        .as_ref()
        .map_or(false, |s| js_signal_is_aborted(ctx, s));
    let notify = signal_val.as_ref().and_then(|s| signal_notify(s));

    let url_for_closure = url_str.clone();

    let req = TransportRequest {
        method,
        url: url_str,
        headers: req_headers.to_vec(),
        body: req_body,
        timeout,
        redirect,
        referrer: String::from("about:client"),
    };

    let result_slot = PromiseResultSlot::<Result<TransportResponse, RequestError>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), move |cx, data| -> JSResult {
            let tr = match data {
                Ok(tr) => tr,
                Err(RequestError::Abort) => return throw_abort_error(cx),
                Err(RequestError::Other(msg)) => {
                    cx.throw_internal_error(&msg);
                    return Err(-1);
                }
            };
            build_response_from_transport(cx, tr, "basic", &url_for_closure, false)
        })?;

    if already_aborted {
        let _ = result_slot.commit(Err(RequestError::Abort));
        return Ok(promise.into());
    }

    match notify {
        Some(n) => {
            tokio::spawn(async move {
                let fut = perform_request(&client, req);
                tokio::select! {
                    r = fut => { let _ = result_slot.commit(r.map_err(RequestError::Other)); }
                    _ = n.notified() => { let _ = result_slot.commit(Err(RequestError::Abort)); }
                }
            });
        }
        None => {
            tokio::spawn(async move {
                let r = perform_request(&client, req)
                    .await
                    .map_err(RequestError::Other);
                let _ = result_slot.commit(r);
            });
        }
    }

    Ok(promise.into())
}

/// Check whether a JS AbortSignal is aborted.
///
/// Uses the native-object payload when available.  When the value was stored
/// as a JS property and retrieved via `get_property_str`, zuqe may lose the
/// native-object pointer; in that case we create a small native function once
/// Check whether a JS AbortSignal is aborted.
///
/// Only the native-object payload path works; the `get_property_str` fallback
/// is deliberately omitted because zuqe loses the native-object pointer for
/// values stored as JS properties (a known engine limitation – see notes in
/// whatwg/fetch/mod.rs where the same check succeeds because the init object
/// is accessed differently).
fn js_signal_is_aborted(_ctx: &mut ScopedContext, val: &JSValue) -> bool {
    JSAbortSignal::payload_of(val)
        .map(|p| p.is_aborted())
        .unwrap_or(false)
}

// ── Prototype ──────────────────────────────────────────────────────────────

const CLASS_HTTP_CLIENT_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("request", 2, js_do_request, METHOD_FROM_OPTS),
    FieldDef::FUNC("get", 2, js_do_request, M_GET),
    FieldDef::FUNC("post", 2, js_do_request, M_POST),
    FieldDef::FUNC("put", 2, js_do_request, M_PUT),
    FieldDef::FUNC("delete", 2, js_do_request, M_DELETE),
    FieldDef::FUNC("patch", 2, js_do_request, M_PATCH),
];

def_native_object!(
    "HttpClient",
    JSHttpClient,
    HttpClientPayload,
    CLASS_HTTP_CLIENT_PROTO,
    Some(ClassCtorDef::new(js_constructor, 1, true))
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_http_client(rt: &mut Runtime) -> Result<zuqe::ClassID, i32> {
    JSHttpClient::register(rt)
}
