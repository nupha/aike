// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Fetch API — `fetch()`, `Headers`, `Request`, `Response`.
//!
//! All four are implemented here and share the header/body/transport
//! helpers in this module. The client `Response` is also reused by the
//! legacy `request()` global.

pub(crate) mod body;
pub(crate) mod headers;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod transport;

pub(crate) use request::JSRequest;

use std::time::Duration;

use zuqe::{CallContext, Callback, ErrorKind, Exception, JSResult, JSValue, Object, ScopedContext};

use crate::whatwg::abort::{JSAbortSignal, signal_is_aborted, signal_notify};
use crate::whatwg::fetch::body::body_to_bytes;
use crate::whatwg::fetch::headers::{HeaderList, header_list_from_js};
use crate::whatwg::fetch::response::{
    build_opaque_redirect_response, build_response_from_transport,
};
use crate::whatwg::fetch::transport::{TransportRequest, TransportResponse, perform_request};

/// Error type carried through the fetch result slot.
enum FetchError {
    /// Aborted via `AbortSignal` (or pre-aborted signal).
    Abort,
    /// Any other failure (network, redirect, timeout, ...).
    Other(String),
}

/// Throw a WHATWG-conformant `AbortError` (a `TypeError` whose `name` is
/// "AbortError"), as required when a fetch is cancelled by a signal.
fn throw_abort_error(cx: &mut ScopedContext) -> JSResult {
    let mut err = match cx.create_error_object(ErrorKind::Type, "The operation was aborted", false) {
        Some(e) => e,
        None => return Err(-1),
    };
    let name = cx.create_string_from_utf8(b"AbortError".to_vec())?;
    err.define_property_value_str(
        cx,
        "name",
        name.into(),
        zuqe::JS_PROP_WRITABLE | zuqe::JS_PROP_CONFIGURABLE,
    )?;
    cx.throw(Exception::Error(err.into()));
    Err(-1)
}

/// `fetch(input, init)` → `Promise<Response>`
pub(crate) fn js_fetch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let input = cc.arg(0);
    let init = if cc.length() > 1 {
        cc.arg(1)
    } else {
        JSValue::UNDEFINED
    };

    let mut url_str = String::new();
    let mut method = String::from("GET");
    let mut header_list = HeaderList::new();
    let mut body_bytes: Option<Vec<u8>> = None;
    let mut redirect = String::from("follow");
    let mut timeout = Duration::from_secs(30);
    
    // RequestInit fields (forwarded to transport; some map to reqwest behavior).
    let mut mode = String::from("cors");
    let mut credentials = String::from("same-origin");
    let mut cache = String::from("default");
    let mut referrer = String::from("about:client");
    let mut referrer_policy = String::new();
    let mut integrity = String::new();
    let mut keepalive = false;
    let mut signal_val: Option<JSValue> = None;

    // Inherit from a Request input.
    if let Some(p) = input.as_native_object::<JSRequest>().and_then(|o| o.payload()) {
        url_str = p.url.clone();
        method = p.method.clone();
        header_list = p.headers.clone();
        redirect = p.redirect.clone();
        signal_val = p.signal.clone();
        body_bytes = p.synthetic_body.clone();
        mode = p.mode.clone();
        credentials = p.credentials.clone();
        cache = p.cache.clone();
        referrer = p.referrer.clone();
        referrer_policy = p.referrer_policy.clone();
        integrity = p.integrity.clone();
        keepalive = p.keepalive;
    } else if !input.is_undefined_or_null() {
        let href = input.get_property_str(ctx, "href")?;
        if !href.is_undefined() {
            url_str = href.to_string(ctx)?.content().encode_utf8(false).to_string();
        } else {
            url_str = input.to_string(ctx)?.content().encode_utf8(false).to_string();
        }
    }

    // Apply RequestInit overrides.
    if !init.is_undefined_or_null() {
        if let Some(o) = init.as_object() {
            let m = o.get_property_str(ctx, "method")?;
            if !m.is_undefined() {
                method = m
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string()
                    .to_uppercase();
            }
            let mut h = o.get_property_str(ctx, "headers")?;
            if !h.is_undefined_or_null() {
                header_list = header_list_from_js(ctx, &mut h)?;
            }
            let b = o.get_property_str(ctx, "body")?;
            if !b.is_undefined_or_null() {
                body_bytes = Some(body_to_bytes(ctx, &b)?);
            }
            let r = o.get_property_str(ctx, "redirect")?;
            if !r.is_undefined() {
                redirect = r.to_string(ctx)?.content().encode_utf8(false).to_string();
            }
            let s = o.get_property_str(ctx, "signal")?;
            if !s.is_undefined_or_null() {
                if s.as_native_object::<JSAbortSignal>().is_some() {
                    signal_val = Some(s.clone());
                }
            }
            let t = o.get_property_str(ctx, "timeout")?;
            if !t.is_undefined() {
                let secs = t.to_number(ctx)?.to_f64();
                if secs > 0.0 {
                    timeout = Duration::from_secs_f64(secs);
                }
            }
            
            let mut read_str = |o: &Object, key: &str| -> Option<String> {
                let v = o.get_property_str(ctx, key).ok()?;
                if v.is_undefined() {
                    None
                } else {
                    Some(v.to_string(ctx).ok()?.content().encode_utf8(false).to_string())
                }
            };
            if let Some(v) = read_str(o, "mode") {
                mode = v;
            }
            if let Some(v) = read_str(o, "credentials") {
                credentials = v;
            }
            if let Some(v) = read_str(o, "cache") {
                cache = v;
            }
            if let Some(v) = read_str(o, "referrer") {
                referrer = v;
            }
            if let Some(v) = read_str(o, "referrerPolicy") {
                referrer_policy = v;
            }
            if let Some(v) = read_str(o, "integrity") {
                integrity = v;
            }
            let ka = o.get_property_str(ctx, "keepalive")?;
            if !ka.is_undefined() {
                keepalive = ka.to_number(ctx).map(|n| n.to_f64() != 0.0).unwrap_or(false);
            }
        }
    }

    let notify = signal_val.as_ref().and_then(|s| signal_notify(s));
    let already_aborted = signal_val.as_ref().map_or(false, |s| signal_is_aborted(s));

    let result_slot = zuqe::builtins::promise::PromiseResultSlot::<
        Result<TransportResponse, FetchError>,
    >::new();
    let url_clone = url_str.clone();
    let redirect_clone = redirect.clone();
    let promise = zuqe::builtins::promise::Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, data| -> JSResult {
            let tr = match data {
                Ok(tr) => tr,
                Err(e) => {
                    return match e {
                        FetchError::Abort => throw_abort_error(cx),
                        FetchError::Other(msg) => {
                            cx.throw_internal_error(&msg);
                            Err(-1)
                        }
                    };
                }
            };
            let status = tr.status;
            if (redirect_clone == "error" || redirect_clone == "manual") && (300..400).contains(&status) {
                if redirect_clone == "error" {
                    cx.throw_type_error("Failed to fetch: redirect not allowed (mode 'error')");
                    return Err(-1);
                }
                return build_opaque_redirect_response(cx);
            }
            build_response_from_transport(cx, tr, "basic", &url_clone, false)
        },
    )?;

    if already_aborted {
        let _ = result_slot.commit(Err(FetchError::Abort));
        return Ok(promise.into());
    }

    let req = TransportRequest {
        method,
        url: url_str.clone(),
        headers: header_list.to_vec(),
        body: body_bytes,
        timeout,
        redirect: redirect.clone(),
        credentials,
        referrer,
        integrity,
        mode,
        cache,
        referrer_policy,
        keepalive,
    };
    let rs = result_slot.clone();

    match notify {
        Some(n) => {
            tokio::spawn(async move {
                let fut = perform_request(req);
                tokio::select! {
                    r = fut => { let _ = rs.commit(r.map_err(FetchError::Other)); }
                    _ = n.notified() => { let _ = rs.commit(Err(FetchError::Abort)); }
                }
            });
        }
        None => {
            tokio::spawn(async move {
                let r = perform_request(req).await.map_err(FetchError::Other);
                let _ = rs.commit(r);
            });
        }
    }

    Ok(promise.into())
}

/// Install the `fetch` global and `Response` statics.
pub(crate) fn install_fetch(ctx: &mut ScopedContext, mut global: Object) -> Result<(), i32> {
    let func = ctx.create_native_function(Callback::Func(js_fetch), "fetch", 1, 0)?;
    global.define_property_value_str(ctx, "fetch", func.into(), zuqe::JS_PROP_C_W_E)?;
    crate::whatwg::fetch::response::register_response_statics(ctx, &mut global)?;
    Ok(())
}
