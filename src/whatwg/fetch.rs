// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

use zuqe::{CallContext, Callback, ErrorKind, Exception, JSResult, JSValue, Object, ScopedContext};

use crate::http::client::{
    body::body_to_bytes,
    cache as http_cache,
    headers::{HeaderList, header_list_from_js},
    request::JSRequest,
    response::{RespType, build_opaque_redirect_response, build_response_from_transport},
    transport::{
        TransportError, TransportRequest, TransportResponse, perform_request, throw_transport_error,
    },
};

use super::{
    abort::{JSAbortSignal, signal_is_aborted, signal_notify, signal_set_body},
    blob::blob_type_of,
    stream::JSReadableStream,
};

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
    // Browsers always send a User-Agent; WPT checks for its presence.
    if !h.has("user-agent") {
        h.append("User-Agent", "Mozilla/5.0 (aike)");
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
    // Fetch spec default request mode is "cors"; "no-cors" produces an opaque
    // response for cross-origin fetches.
    let mut mode = String::from("cors");
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
        mode = p.mode.clone();
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

    // Fetch spec: the request URL is serialized "with the exclude fragment
    // flag set" — the fragment is never sent and never appears in
    // `response.url`. Parsing also normalizes the URL (e.g. an empty path
    // becomes "/"). Leave unparsable URLs alone so the transport reports them.
    if let Ok(mut u) = url::Url::parse(&url_str) {
        u.set_fragment(None);
        url_str = u.to_string();
    } else if let Some(base) = ctx.current_module_name() {
        // Relative specifier: resolve against the current module's base URL
        // (the value behind `import.meta.url`, set by `aike.evalModule`),
        // mirroring Node/Bun semantics. `None` when not inside a script/module
        // frame, in which case the specifier stays as-is.
        if let Ok(base_u) = url::Url::parse(&base) {
            if let Ok(joined) = base_u.join(&url_str) {
                let mut u = joined;
                u.set_fragment(None);
                url_str = u.to_string();
            }
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
                if JSReadableStream::payload_of(&b).is_some() {
                    body_stream = Some(b.clone());
                } else {
                    // Fetch spec: a GET/HEAD request must not carry a body.
                    if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
                        return Err(
                            ctx.throw_type_error("Request with GET/HEAD method cannot have body")
                        );
                    }
                    // WHATWG: a Blob body without an explicit Content-Type gets
                    // its type propagated to the Content-Type request header.
                    if let Some(t) = blob_type_of(&b) {
                        if !t.is_empty() && !header_list.has("content-type") {
                            header_list.set("Content-Type", &t);
                        }
                    }
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
            if let Some(v) = read_str(o, "mode") {
                mode = v;
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

    // HTTP cache: if we have a stored validator for this URL, attach conditional
    // revalidation headers so the server may reply `304` (Fetch § HTTP-cache).
    // `cache_driven` records whether *we* added the validators — only then must
    // a `304` be reinterpreted as the cached response (caller-supplied headers
    // must keep the response as a real `304`). It is computed inside the
    // response closure (before the 304 handling) from the cached validators.

    let result_slot =
        zuqe::builtins::promise::PromiseResultSlot::<Result<TransportResponse, FetchError>>::new();
    // Look up cached validators before issuing the request so the conditional
    // headers ride on the outgoing request and the `cache_driven` flag (copied
    // into the response closure below) reflects *our* revalidation.
    let cached_validators = if method.eq_ignore_ascii_case("GET") {
        http_cache::lookup_validators(&method, &url_str)
    } else {
        None
    };
    // Only treat a `304` as cache-driven (to be turned into the cached
    // response) when *we* added the validators — if the caller supplied them
    // the `304` is theirs and must be passed through unchanged.
    let cache_driven = cached_validators.is_some()
        && !http_cache::caller_supplied_validator(&header_list.to_vec());
    let url_clone = url_str.clone();
    let redirect_clone = redirect.clone();
    let method_clone = method.clone();
    let mode_clone = mode.clone();

    // Client origin for same-origin checks. In a browser this is the document's
    // origin; here we read the `self.location` shim that WPT runners inject (and
    // Node/Bun WPT runners do the same). When absent, treat as a null origin so
    // every URL is cross-origin (no-cors then yields an opaque response).
    let client_origin: Option<String> = ctx
        .global_obj()
        .get_property_str(ctx, "self")
        .ok()
        .and_then(|selfv| selfv.get_property_str(ctx, "location").ok())
        .and_then(|loc| loc.get_property_str(ctx, "origin").ok())
        .and_then(|o| {
            if o.is_undefined_or_null() {
                None
            } else {
                Some(
                    o.to_string(ctx)
                        .ok()?
                        .content()
                        .encode_utf8(false)
                        .to_string(),
                )
            }
        })
        .filter(|s| !s.is_empty());

    // Clone the client origin for request-header injection; the original is
    // moved into the response callback closure below.
    let req_origin = client_origin.clone();

    let promise = zuqe::builtins::promise::Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, data| -> JSResult {
            let client_origin = client_origin.clone();
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

            if matches!(redirect_clone.as_str(), "error" | "manual") && (300..400).contains(&status)
            {
                if redirect_clone == "error" {
                    return Err(
                        cx.throw_type_error("Failed to fetch: redirect not allowed (mode 'error')")
                    );
                }
                return build_opaque_redirect_response(cx);
            }

            // HTTP cache: a `304` we drove via conditional headers becomes the
            // cached response (status 200) with the 304 headers merged on top.
            if let Some((cached_status, cached_headers, cached_body)) =
                http_cache::take_not_modified(
                    &method_clone,
                    &url_clone,
                    status,
                    &tr.headers,
                    cache_driven,
                )
            {
                return build_response_from_transport(
                    cx,
                    TransportResponse {
                        status: cached_status,
                        status_text: "OK".to_string(),
                        headers: cached_headers,
                        content_type: String::new(),
                        content_length: Some(cached_body.len() as u64),
                        http_version: "1.1",
                        resp: None,
                        synthetic_body: Some(cached_body),
                    },
                    RespType::Basic,
                    &url_clone,
                    false,
                );
            }
            // Otherwise remember successful validator-bearing GET responses so
            // later requests can revalidate them.
            if method_clone.eq_ignore_ascii_case("GET") && (200..300).contains(&status) {
                http_cache::store(&method_clone, &url_clone, status, &tr.headers, Vec::new());
            }
            // A `no-cors` request for a cross-origin (or unknown-origin) URL
            // yields an opaque response per Fetch spec (status 0, type opaque,
            // null body, CORS-filtered headers). Same-origin stays basic.
            let is_opaque = mode_clone == "no-cors"
                && url::Url::parse(&url_clone)
                    .map(|u| u.origin().ascii_serialization())
                    .ok()
                    .map_or(true, |req_origin| {
                        client_origin.as_deref() != Some(req_origin.as_str())
                    });
            let resp_type = if is_opaque {
                RespType::Opaque
            } else {
                RespType::Basic
            };
            build_response_from_transport(cx, tr, resp_type, &url_clone, false)
        },
    )?;

    if already_aborted {
        let _ = result_slot.commit(Err(FetchError::Abort));
        return Ok(promise.into());
    }

    // Per-host TLS escape hatch: skip cert validation only for hosts listed in
    // AIKE_TLS_INSECURE_HOSTS (self-signed dev/WPT servers). All other hosts
    // keep strict validation.
    let insecure = url::Url::parse(&url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| (h.to_string(), u.port())))
        .map(|(h, p)| crate::http::client::config::tls_insecure_for_host(&h, p))
        .unwrap_or(false);

    // Per-host TLS escape hatch: `insecure` skips cert validation for this
    // request's host (AIKE_TLS_INSECURE_HOSTS).
    let client = match crate::http::client::config::HttpClientConfig::default()
        .build_http_client_ex(insecure)
    {
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

    // `Origin` is a forbidden request header (scripts cannot set it), so it is
    // added here *after* the forbidden-header strip. Safe/idempotent methods
    // (`GET`, `HEAD`) do not carry an `Origin` header; any other method does.
    // The value is the client's origin, or the literal `null` when the client
    // origin is opaque.
    if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
        match &req_origin {
            Some(o) => header_list.set("Origin", o),
            None => header_list.set("Origin", "null"),
        }
    }

    // Snapshot after the HTTP-default and forbidden-header passes so the
    // conditional-revalidation headers added below ride on the final list.
    let mut req_headers = header_list.to_vec();
    // Apply conditional-revalidation validators (from the HTTP cache) to the
    // finalized request headers; this must run after the forbidden-header
    // strip above so it cannot revive a dropped header.
    if let Some(validators) = &cached_validators {
        http_cache::add_conditional_headers(&mut req_headers, validators.clone());
    }

    let req = TransportRequest {
        method,
        url: url_str.clone(),
        headers: req_headers,
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
