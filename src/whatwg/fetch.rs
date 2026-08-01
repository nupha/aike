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

use super::abort::{JSAbortSignal, signal_is_aborted, signal_notify, signal_set_body};
use super::stream::JSReadableStream;

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
///
/// Per the Fetch spec `fetch()` never throws synchronously: request-construction
/// failures (bad URL, invalid header, ...) must surface as a rejected promise.
/// `fetch_impl` does the real work and may throw; here we catch a pending
/// exception and convert it into a rejection.
pub(crate) fn js_fetch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    match fetch_impl(ctx, cc) {
        Ok(v) => Ok(v),
        Err(e) => {
            // Turn the in-flight exception into a rejected promise.
            let Exception::Error(reason) = ctx.take_exception() else {
                // Nothing retrievable (e.g. stack overflow): propagate as-is.
                return Err(e);
            };
            let (mut promise, _funcs) = zuqe::builtins::promise::Promise::create(ctx)?;
            promise.reject(ctx, reason);
            Ok(promise.into())
        }
    }
}

// Inject Fetch-spec default request headers (`Accept`, `Accept-Language`)
// when the caller did not supply them. Called after RequestInit/Request
// inheritance is applied, so explicit headers always win.
fn ensure_default_request_headers(h: &mut HeaderList) {
    if !h.has("accept") {
        h.append("Accept", "*/*");
    }
    if !h.has("accept-language") {
        h.append("Accept-Language", "*");
    }
}

/// WHATWG "forbidden request-header" names: a `fetch()` must silently drop any
/// header whose name (case-insensitively) is in this set, or begins with
/// `Proxy-` / `Sec-`. The browser (or underlying transport) owns these; a
/// script must not be able to set them. (Set-Cookie is also forbidden as a
/// request header but Headers does not expose it for sending anyway.)
///
/// The three `x-*` method-override headers are *conditionally* forbidden: only
/// when their value resolves to a forbidden method (see
/// `is_forbidden_override_value`).
fn is_forbidden_request_header(name: &str, value: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "accept-charset",
        "accept-encoding",
        "access-control-request-headers",
        "access-control-request-method",
        "connection",
        "content-length",
        "cookie",
        "cookie2",
        "date",
        "dnt",
        "expect",
        "host",
        "keep-alive",
        "origin",
        "referer",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "via",
    ];
    let lower = name.to_lowercase();
    if FORBIDDEN.contains(&lower.as_str()) {
        return true;
    }
    if lower.starts_with("proxy-") || lower.starts_with("sec-") {
        return true;
    }
    // `x-http-method` / `x-http-method-override` / `x-method-override`: forbidden
    // only when the value resolves to a forbidden method (CONNECT/TRACE/TRACK).
    if matches!(
        lower.as_str(),
        "x-http-method" | "x-http-method-override" | "x-method-override"
    ) {
        return is_forbidden_override_value(value);
    }
    false
}

/// WHATWG Fetch: the three method-override headers are forbidden when their
/// value, after "get, decode, and split" (split on commas, with quoted strings
/// kept intact and each item trimmed), yields a forbidden method
/// (CONNECT/TRACE/TRACK, byte-case-insensitive). A quoted method like
/// `"TRACE"` does NOT match (the quotes are part of the token), so a value such
/// as `",TRACE,"` is permitted.
fn is_forbidden_override_value(value: &str) -> bool {
    for method in split_on_commas(value) {
        if matches!(
            method.to_ascii_lowercase().as_str(),
            "connect" | "trace" | "track"
        ) {
            return true;
        }
    }
    false
}

/// Fetch "get, decode, and split" for a header value: split on commas, but a
/// comma inside an HTTP quoted string (`"..."`) is not a separator, and each
/// item is trimmed of surrounding whitespace. Quoted strings keep their quote
/// characters (so `"TRACE"` is a distinct token from `TRACE`). Backslash
/// escapes inside a quoted string are preserved verbatim.
fn split_on_commas(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let mut pos = 0;
    let mut out: Vec<String> = Vec::new();
    let mut temp = String::new();
    loop {
        while pos < chars.len() && chars[pos] != '"' && chars[pos] != ',' {
            temp.push(chars[pos]);
            pos += 1;
        }
        if pos < chars.len() && chars[pos] == '"' {
            // Collect the HTTP quoted string, keeping the quote characters.
            temp.push('"');
            pos += 1;
            while pos < chars.len() {
                let c = chars[pos];
                temp.push(c);
                pos += 1;
                if c == '"' {
                    break;
                }
                if c == '\\' && pos < chars.len() {
                    temp.push(chars[pos]);
                    pos += 1;
                }
            }
            if pos < chars.len() {
                continue;
            }
        }
        out.push(temp.trim().to_string());
        temp.clear();
        if pos >= chars.len() {
            return out;
        }
        if chars[pos] == ',' {
            pos += 1;
        }
    }
}

/// Drop all forbidden request headers set by the caller. Called after default
/// headers are injected (Accept / Accept-Language are not forbidden, so they
/// survive). The transport layer adds its own Content-Length / Host later.
fn strip_forbidden_request_headers(h: &mut HeaderList) {
    let drop: Vec<String> = h
        .to_vec()
        .into_iter()
        .filter(|(k, v)| is_forbidden_request_header(k, v))
        .map(|(k, _)| k)
        .collect();
    for k in drop {
        h.delete(&k);
    }
}

fn fetch_impl(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
    // A `ReadableStream` body is uploaded as-is (streaming). We must keep the
    // JS stream alive so that, per the Fetch spec, an abort can cancel it with
    // the abort reason (see the abort branch below).
    let mut body_stream: Option<JSValue> = None;
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

    // Fetch spec: a request URL containing a NUL byte is a network error
    // (TypeError), not a transport "request failed" error.
    if url_str.contains('\0') {
        return throw_transport_error(
            ctx,
            TransportError {
                code: "other",
                message: "URL contains a null byte".into(),
            },
        );
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
                if JSReadableStream::payload_of(&b).is_some() {
                    body_stream = Some(b.clone());
                } else {
                    body_bytes = Some(body_to_bytes(ctx, &b)?);
                }
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

    // A `ReadableStream` body is canceled with the abort reason when the signal
    // aborts (Fetch spec). Attach it to the signal; `js_controller_abort` cancels
    // it synchronously with a live `cx`, avoiding any cross-async JSValue handle.
    if let (Some(stream), Some(signal)) = (body_stream.as_ref(), signal_val.as_ref()) {
        signal_set_body(signal, *stream);
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
                    return Err(
                        cx.throw_type_error("Failed to fetch: redirect not allowed (mode 'error')")
                    );
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
            return Err(ctx.throw_internal_error(&format!("failed to build HTTP client: {e}")));
        }
    };

    // Fetch spec "HTTP defaults": a request carries default `Accept` and
    // `Accept-Language` headers unless the caller overrides them. reqwest's
    // own defaults are wiped by `perform_request` (it sets each header
    // explicitly), so we must supply them here.
    ensure_default_request_headers(&mut header_list);
    // WHATWG: silently drop forbidden request headers the caller tried to set.
    strip_forbidden_request_headers(&mut header_list);

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
