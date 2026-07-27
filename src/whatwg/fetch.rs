// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

use zuqe::{CallContext, Callback, ErrorKind, Exception, JSResult, JSValue, Object, ScopedContext};

use crate::http::client::{
    body::body_to_bytes,
    headers::{HeaderList, header_list_from_js},
    request::JSRequest,
    response::{RespType, build_opaque_redirect_response, build_response_from_transport},
    transport::{
        TransportError, TransportRequest, TransportResponse, perform_request, throw_transport_error,
    },
};

use super::abort::{JSAbortSignal, signal_is_aborted, signal_notify};

/// Error type carried through the fetch result slot.
enum FetchError {
    /// Aborted via `AbortSignal` (or pre-aborted signal).
    Abort,
    /// Any other failure (network, redirect, timeout, ...).
    Other(TransportError),
}

/// Throw a WHATWG-conformant `AbortError` (a `TypeError` whose `name` is
/// "AbortError"), as required when a fetch is cancelled by a signal.
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

    // RequestInit fields used for client config.
    let mut credentials = String::from("same-origin");
    let mut referrer = String::from("about:client");
    let mut signal_val: Option<JSValue> = None;

    // Inherit from a Request input.
    if let Some(p) = JSRequest::payload_of(&input) {
        url_str = p.url.clone();
        method = p.method.clone();
        header_list = p.headers.clone();
        redirect = p.redirect.clone();
        signal_val = p.signal.clone();
        body_bytes = p.synthetic_body.clone();
        credentials = p.credentials.clone();
        referrer = p.referrer.clone();
    } else if !input.is_undefined_or_null() {
        let href = input.get_property_str(ctx, "href")?;
        if !href.is_undefined() {
            url_str = href
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
        } else {
            url_str = input
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
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

            let mut read_str = |o: Object, key: &str| -> Option<String> {
                let v = o.get_property_str(ctx, key).ok()?;
                if v.is_undefined() {
                    None
                } else {
                    Some(
                        v.to_string(ctx)
                            .ok()?
                            .content()
                            .encode_utf8(false)
                            .to_string(),
                    )
                }
            };
            if let Some(v) = read_str(o, "credentials") {
                credentials = v;
            }
            if let Some(v) = read_str(o, "referrer") {
                referrer = v;
            }
        }
    }

    let notify = signal_val.as_ref().and_then(|s| signal_notify(s));
    let already_aborted = signal_val.as_ref().map_or(false, |s| signal_is_aborted(s));

    let result_slot =
        zuqe::builtins::promise::PromiseResultSlot::<Result<TransportResponse, FetchError>>::new();
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
                        FetchError::Other(te) => throw_transport_error(cx, te),
                    };
                }
            };
            let status = tr.status;
            if (redirect_clone == "error" || redirect_clone == "manual")
                && (300..400).contains(&status)
            {
                if redirect_clone == "error" {
                    cx.throw_type_error("Failed to fetch: redirect not allowed (mode 'error')");
                    return Err(-1);
                }
                return build_opaque_redirect_response(cx);
            }
            build_response_from_transport(cx, tr, RespType::Basic, &url_clone, false)
        },
    )?;

    if already_aborted {
        let _ = result_slot.commit(Err(FetchError::Abort));
        return Ok(promise.into());
    }

    // Build a reqwest::Client with redirect policy and cookie jar.
    let redirect_policy = match redirect.as_str() {
        "error" | "manual" => reqwest::redirect::Policy::none(),
        _ => reqwest::redirect::Policy::limited(10),
    };
    let mut builder = reqwest::Client::builder().redirect(redirect_policy);
    if credentials == "include" || credentials == "same-origin" {
        builder = builder.cookie_store(true);
    }
    let client = match builder.build() {
        Ok(c) => c,
        Err(e) => {
            ctx.throw_internal_error(&format!("failed to build HTTP client: {e}"));
            return Err(-1);
        }
    };

    let req = TransportRequest {
        method,
        url: url_str.clone(),
        headers: header_list.to_vec(),
        body: body_bytes,
        timeout,
        referrer,
    };

    let rs = result_slot.clone();
    let notify2 = notify;

    match notify2 {
        Some(n) => {
            tokio::spawn(async move {
                let fut = perform_request(&client, req);
                tokio::select! {
                    r = fut => { let _ = rs.commit(r.map_err(FetchError::Other)); }
                    _ = n.notified() => { let _ = rs.commit(Err(FetchError::Abort)); }
                }
            });
        }
        None => {
            tokio::spawn(async move {
                let r = perform_request(&client, req)
                    .await
                    .map_err(FetchError::Other);
                let _ = rs.commit(r);
            });
        }
    }

    Ok(promise.into())
}

/// Install the `fetch` global and register all WHATWG classes.
pub(crate) fn install_fetch(ctx: &mut ScopedContext, mut global: Object) -> Result<(), i32> {
    let func = ctx.create_native_function(Callback::Func(js_fetch), "fetch", 1, 0)?;
    global.define_property_value_str(ctx, "fetch", func.into(), zuqe::JS_PROP_C_W_E)?;

    Ok(())
}
