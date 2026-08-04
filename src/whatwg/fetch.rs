// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::RefCell, rc::Rc, time::Duration};

use zuqe::{
    CallContext, Callback, Exception, JSResult, JSValue, JobState, Object, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::http::client::{
    body::{body_content_type, body_to_bytes, drain_readable_stream},
    cache as http_cache,
    cors::{
        check_preflight, cors_check, cors_filter_response_headers, is_cross_origin, needs_preflight,
        preflight_cache_hit, preflight_cache_key, preflight_cache_store,
        preflight_request_headers, serialized_origin, should_append_origin,
    },
    headers::{HeaderList, header_list_from_js, validate_header_list_for_fetch},
    request::{JSRequest, get_request_payload_mut},
    response::{RespType, build_opaque_redirect_response, build_response_from_transport},
    transport::{
        PreflightRequest, ReferrerCtx, TransportError, TransportRequest, TransportResponse,
        perform_preflight, perform_request, throw_transport_error,
    },
};

use super::{
    abort::{JSAbortSignal, signal_is_aborted, signal_notify, signal_reason},
    dom::make_dom_exception,
    stream::{JSReadableStream, make_rejected_promise, make_resolved_promise},
};

/// Error type carried through the fetch result slot.
enum FetchError {
    /// Aborted via `AbortSignal` (or pre-aborted signal).
    Abort,
    /// Any other failure (network, redirect, timeout, ...).
    Other(TransportError),
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
            let (mut promise, _funcs) = Promise::create(ctx)?;
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
/// Compute the outgoing `Referer` value per the Fetch `referrer`/`referrerPolicy`
/// algorithm.
///
/// `referrer` is the explicit `RequestInit.referrer` (or `"about:client"`).
/// `referrer_policy` is the explicit `referrerPolicy` string (empty = default).
/// `client_origin`/`client_url` come from the `self.location` shim (browser
/// document origin / URL). `request_url` is the serialized request URL.
///
/// Covers the policies exercised by WPT `referrer.any.js`:
/// `origin-when-cross-origin` and the implicit default, plus explicit `referrer`
/// values whose credentials and fragment are stripped to yield the request's
/// origin (+ trailing slash).
pub(crate) fn compute_request_referrer(
    referrer: &str,
    referrer_policy: &str,
    client_origin: Option<&str>,
    client_url: Option<&str>,
    request_url: &str,
) -> Option<String> {
    // Strip any fragment and trailing `#` from a URL string.
    let strip_fragment = |u: &str| -> String {
        match u.find('#') {
            Some(i) => u[..i].to_string(),
            None => u.to_string(),
        }
    };
    // Origin (scheme://host[:port]) of a URL, or None if unparsable.
    let origin_of = |u: &str| -> Option<String> {
        url::Url::parse(u)
            .ok()
            .map(|u| u.origin().ascii_serialization())
    };
    // Whether a URL is "potentially trustworthy" enough to send a referrer to
    // over a (non-)downgrade. http://localhost and https are trustworthy.
    let trustworthy = |u: &str| -> bool {
        match url::Url::parse(u) {
            Ok(parsed) => {
                parsed.scheme() == "https"
                    || (parsed.scheme() == "http"
                        && matches!(parsed.host_str(), Some("localhost") | Some("127.0.0.1")))
            }
            Err(_) => false,
        }
    };

    // `no-referrer` policy always yields an empty referrer.
    if referrer_policy == "no-referrer" {
        return Some(String::new());
    }

    // Resolve the referrer source URL. `"about:client"` (or empty) uses the
    // document URL; an explicit `referrer` may be absolute or relative and is
    // resolved against the document URL, then stripped of its fragment.
    let source: Option<String> = if referrer.is_empty() || referrer == "about:client" {
        client_url.map(|u| strip_fragment(u))
    } else {
        let resolved = if let Ok(u) = url::Url::parse(referrer) {
            u.to_string()
        } else if let Some(base) = client_url {
            url::Url::parse(base)
                .ok()
                .and_then(|b| b.join(referrer).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| referrer.to_string())
        } else {
            referrer.to_string()
        };
        Some(strip_fragment(&resolved))
    };
    let source = source?;

    let req_origin = origin_of(request_url);
    let same_origin = match (&req_origin, client_origin) {
        (Some(ro), Some(co)) => ro == co,
        _ => false,
    };

    // Helper: origin of `source` with trailing slash (e.g. "http://h:8000/").
    let source_origin_slash = || -> Option<String> {
        let o = origin_of(&source)?;
        Some(if o.ends_with('/') { o } else { format!("{o}/") })
    };

    // Fetch spec "strip": if the referrer carries credentials (userinfo), it
    // collapses to its origin (+ trailing slash) regardless of policy.
    if url::Url::parse(&source)
        .map(|u| u.username() != "" || u.password().is_some())
        .unwrap_or(false)
    {
        return source_origin_slash();
    }

    let full = || url::Url::parse(&source).map(|u| u.to_string()).unwrap_or(source.clone());

    // Apply the policy (Fetch spec referrer policy table).
    match referrer_policy {
        "origin" => source_origin_slash(),
        "same-origin" => {
            if same_origin {
                Some(source)
            } else {
                Some(String::new())
            }
        }
        "origin-when-cross-origin" => {
            if same_origin {
                Some(source)
            } else {
                source_origin_slash()
            }
        }
        "strict-origin" => {
            if trustworthy(&source) {
                source_origin_slash()
            } else {
                Some(String::new())
            }
        }
        "strict-origin-when-cross-origin" => {
            if same_origin {
                Some(source)
            } else if trustworthy(&source) {
                source_origin_slash()
            } else {
                Some(String::new())
            }
        }
        "unsafe-url" => Some(full()),
        // Implicit default (no explicit `referrerPolicy`): behaves like
        // `origin-when-cross-origin` (full referrer same-origin, origin-only
        // cross-origin), which is what WPT's default-policy tests expect.
        "" => {
            if same_origin {
                Some(source)
            } else {
                source_origin_slash()
            }
        }
        // `no-referrer-when-downgrade`: send the full referrer unless the
        // request would be a downgrade (https → http).
        _ => {
            let source_trusted = trustworthy(&source);
            let req_trusted = trustworthy(request_url);
            if source_trusted && !req_trusted {
                Some(String::new())
            } else {
                Some(full())
            }
        }
    }
}

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

/// Hash algorithm named in a Subresource Integrity metadata entry.
enum HashAlg {
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlg {
    /// Parse the algorithm name (case-insensitive, e.g. "sha256").
    fn parse(name: &str) -> Option<HashAlg> {
        match name.to_ascii_lowercase().as_str() {
            "sha256" => Some(HashAlg::Sha256),
            "sha384" => Some(HashAlg::Sha384),
            "sha512" => Some(HashAlg::Sha512),
            _ => None,
        }
    }
    /// Strength ordering — stronger algorithms win per the SRI spec.
    fn strength(&self) -> u8 {
        match self {
            HashAlg::Sha256 => 0,
            HashAlg::Sha384 => 1,
            HashAlg::Sha512 => 2,
        }
    }
}

/// Validate `body` against a Subresource Integrity `integrity` metadata string
/// (space-separated `alg-base64value` tokens). Returns `Err` with a message if
/// the body matches none of the (parsable) entries — per spec, the response is
/// rejected in that case.
fn check_integrity(body: &[u8], integrity: &str) -> Result<(), String> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256, Sha384, Sha512};

    // Collect every well-formed (`alg-value`) entry. A value may be base64
    // (standard, padded or unpadded) or base64url (with or without padding);
    // try each decoder in turn. Entries are kept with their algorithm strength
    // so we can try the strongest first, per the SRI spec.
    let mut entries: Vec<(u8, HashAlg, Vec<u8>)> = Vec::new();
    for tok in integrity.split_whitespace() {
        let (alg_name, b64) = match tok.split_once('-') {
            Some((a, b)) => (a, b),
            None => continue,
        };
        let alg = match HashAlg::parse(alg_name) {
            Some(a) => a,
            None => continue,
        };
        let expected = STANDARD
            .decode(b64)
            .or_else(|_| STANDARD_NO_PAD.decode(b64))
            .or_else(|_| URL_SAFE.decode(b64))
            .or_else(|_| URL_SAFE_NO_PAD.decode(b64));
        let expected = match expected {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        entries.push((alg.strength(), alg, expected));
    }
    if entries.is_empty() {
        return Err(format!(
            "integrity metadata contained no usable entry: {integrity}"
        ));
    }

    // SRI "get the strongest metadata": only entries using the strongest
    // algorithm present are considered. Weaker entries are discarded, so a
    // valid sha384 cannot rescue an invalid sha512 in the same metadata set.
    let strongest = entries.iter().map(|(s, _, _)| *s).max().unwrap_or(0);

    // Within the strongest set any single match is enough to pass.
    for (strength, alg, expected) in &entries {
        if *strength != strongest {
            continue;
        }

        let actual: Vec<u8> = match alg {
            HashAlg::Sha256 => Sha256::digest(body).to_vec(),
            HashAlg::Sha384 => Sha384::digest(body).to_vec(),
            HashAlg::Sha512 => Sha512::digest(body).to_vec(),
        };
        if &actual == expected {
            return Ok(());
        }
    }
    Err(format!(
        "subresource integrity check failed: no entry matched for {}",
        integrity
    ))
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
    // A `ReadableStream` body, held until it can be pumped to bytes below.
    let mut stream_body: Option<JSValue> = None;
    let mut redirect = String::from("follow");
    let mut timeout = Duration::from_secs(30);

    // RequestInit fields used for client config.
    let mut credentials = String::from("same-origin");
    let mut referrer = String::from("about:client");
    let mut referrer_policy = String::from("");
    let mut integrity = String::new();
    // Fetch spec default request mode is "cors"; "no-cors" produces an opaque
    // response for cross-origin fetches.
    let mut mode = String::from("cors");
    let mut signal_val: Option<JSValue> = None;
    // Fetch spec request `cache` mode: default | no-store | reload | no-cache |
    // force-cache | only-if-cached. Drives HTTP cache reuse/revalidation.
    let mut cache_mode = String::from("default");

    // Inherit from a Request input.
    if let Some(p) = JSRequest::payload_of(&input) {
        url_str = p.url.clone();
        method = p.method.clone();
        header_list = p.headers.borrow().clone();
        redirect = p.redirect.clone();
        signal_val = p.signal.clone();
        body_bytes = p.synthetic_body.clone();
        credentials = p.credentials.clone();
        referrer = p.referrer.clone();
        mode = p.mode.clone();
        cache_mode = p.cache.clone();
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
                let raw = m
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                method = crate::http::client::cors::normalize_method(&raw);
            }
            let mut h = o.get_property_str(ctx, "headers")?;
            if !h.is_undefined_or_null() {
                header_list = header_list_from_js(ctx, &mut h)?;
            }
            // `duplex` must be read whenever a body is present: the spec reads
            // it off the init dictionary, and a stream body requires it to be
            // "half". Read it before `body` so a throwing getter still fires.
            let duplex = o.get_property_str(ctx, "duplex")?;
            let duplex = match duplex.is_undefined() {
                true => None,
                false => Some(
                    duplex
                        .to_string(ctx)?
                        .content()
                        .encode_utf8(false)
                        .to_string(),
                ),
            };

            let b = o.get_property_str(ctx, "body")?;
            if !b.is_undefined_or_null() {
                // Fetch spec: a GET/HEAD request must not carry a body.
                if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
                    return Err(
                        ctx.throw_type_error("Request with GET/HEAD method cannot have body")
                    );
                }

                // The body source implies a default Content-Type unless the
                // caller already supplied one.
                if let Some(t) = body_content_type(&b)
                    && !header_list.has("content-type")
                {
                    header_list.set("Content-Type", &t);
                }

                match JSReadableStream::payload_of(&b).is_some() {
                    // A stream body is only allowed with `duplex: "half"`;
                    // omitting it is a TypeError so that code written against
                    // full-duplex semantics fails loudly instead of silently
                    // buffering.
                    true if duplex.as_deref() != Some("half") => {
                        return Err(ctx.throw_type_error(
                            "Request body is a ReadableStream but duplex is not 'half'",
                        ));
                    }
                    true => stream_body = Some(b.clone()),
                    false => body_bytes = Some(body_to_bytes(ctx, &b)?),
                }
            }
            let r = o.get_property_str(ctx, "redirect")?;
            if !r.is_undefined() {
                redirect = r.to_string(ctx)?.content().encode_utf8(false).to_string();
            }
            // Per Fetch spec, an *explicit* `signal: null` in the init clears any
            // signal inherited from the input Request; an absent `signal` key
            // leaves the inherited signal in place.
            let s = o.get_property_str(ctx, "signal")?;
            if s.is_undefined() {
                // Key absent: keep the inherited signal.
            } else if s.is_null() {
                signal_val = None;
            } else if s.as_native_object::<JSAbortSignal>().is_some() {
                signal_val = Some(s.clone());
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
            if let Some(v) = read_str(o, "referrerPolicy") {
                referrer_policy = v;
            }
            if let Some(v) = read_str(o, "integrity") {
                integrity = v;
            }
            if let Some(v) = read_str(o, "mode") {
                mode = v;
            }
            if let Some(v) = read_str(o, "cache") {
                cache_mode = v;
            }
            // Validate `priority` eagerly: an invalid RequestInit member value
            // must reject the fetch with a TypeError (WPT request-init-priority).
            // The valid set is `high` | `low` | `auto`; `high`/`low` are accepted
            // as hints and otherwise ignored, `auto` is the effective default.
            if let Some(v) = read_str(o, "priority") {
                const VALID: &[&str] = &["high", "low", "auto"];
                if !VALID.contains(&v.as_str()) {
                    return Err(ctx.throw_type_error(&format!(
                        "invalid value '{v}' for RequestInit.priority"
                    )));
                }
            }
        }
    }

    // A `ReadableStream` body is canceled with the abort reason when the signal
    // aborts (Fetch spec). Attach it to the signal; `js_controller_abort` cancels
    // it synchronously with a live `cx`, avoiding any cross-async JSValue handle.

    let notify = signal_val.as_ref().and_then(|s| signal_notify(s));
    let already_aborted = signal_val.as_ref().map_or(false, |s| signal_is_aborted(s));
    // When the signal is (already) aborted the fetch must reject with the
    // signal's `reason` (e.g. a DOMException "AbortError"), matching the Fetch
    // spec and WPT's `promise_rejects_exactly`/`promise_rejects_dom`.
    let abort_reason = signal_val
        .as_ref()
        .and_then(|s| signal_reason(s))
        .unwrap_or_else(|| {
            make_dom_exception(ctx, "AbortError", "The operation was aborted.")
                .unwrap_or(JSValue::UNDEFINED)
        });

    // Client origin for same-origin checks (used by the cache short-circuit and
    // the response handler below). In a browser this is the document's origin;
    // here we read the `self.location` shim that WPT runners inject. When absent
    // treat as a null origin so every URL is cross-origin. A failed property read
    // is expected and must not leak a pending exception, so it is taken/discarded.
    let (client_origin, client_url): (Option<String>, Option<String>) = {
        let loc = ctx
            .global_obj()
            .get_property_str(ctx, "self")
            .ok()
            .filter(|selfv| !selfv.is_undefined_or_null())
            .and_then(|selfv| selfv.get_property_str(ctx, "location").ok());
        let origin = loc
            .as_ref()
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
        let url = loc
            .as_ref()
            .and_then(|loc| loc.get_property_str(ctx, "href").ok())
            .and_then(|h| {
                if h.is_undefined_or_null() {
                    None
                } else {
                    Some(
                        h.to_string(ctx)
                            .ok()?
                            .content()
                            .encode_utf8(false)
                            .to_string(),
                    )
                }
            })
            .filter(|s| !s.is_empty());
        (origin, url)
    };

    // CORS tainting: a cross-origin request in `cors` mode. These require
    // origin/ACAO handling and (for non-safelisted requests) a preflight.
    // Computed up front so the response callback (created below) can reference
    // it and the async side can run the preflight.
    let cors_tainted = mode == "cors" && is_cross_origin(client_origin.as_deref(), &url_str);
    // Whether `Access-Control-Allow-Credentials` must be present (fetch
    // `credentials: "include"`).
    let credentials_include = credentials == "include";

    // HTTP cache (§ Fetch HTTP-cache). The request's `cache` mode decides
    // whether we serve straight from cache, revalidate, or always hit network:
    //   default      fresh hit → serve cache; stale hit → revalidate
    //   no-store     always network, never read/write cache
    //   reload       always network (force re-fetch), then re-store
    //   no-cache     always revalidate via conditional headers
    //   force-cache  serve cache hit regardless of freshness; else network
    //   only-if-cached  serve cache hit; a miss is a network error
    // The short-circuit below runs before the network request is issued, so a
    // fresh hit (or `force-cache`) never touches the transport.

    // Is this request cacheable at all (GET, no body)?
    let cacheable =
        method.eq_ignore_ascii_case("GET") && body_bytes.is_none() && stream_body.is_none();

    // A request carrying any conditional header (`If-None-Match`,
    // `If-Modified-Since`, `If-Unmodified-Since`, `If-Match`, `If-Range`) is
    // treated as `no-store`: it bypasses the cache (WPT
    // request-cache-default-conditional).
    let has_conditional = cacheable && http_cache::caller_supplied_validator(&header_list.to_vec());

    // Serve-from-cache hit for the modes that can return without a network call.
    let cache_hit = if cacheable && !has_conditional {
        match cache_mode.as_str() {
            "default" => http_cache::lookup_full(&method, &url_str).filter(|h| h.fresh),
            "force-cache" | "only-if-cached" => http_cache::lookup_full(&method, &url_str),
            _ => None,
        }
    } else {
        None
    };

    // `only-if-cached` with no cached entry is a network error (TypeError).
    if cache_mode == "only-if-cached" && cache_hit.is_none() {
        return throw_transport_error(
            ctx,
            TransportError {
                code: "other",
                message: "only-if-cached: no cached response for the request URL".into(),
            },
        );
    }

    if let Some(hit) = cache_hit {
        // `only-if-cached` requires `mode: "same-origin"`; if the cached
        // response came from a cross-origin redirect, same-origin mode makes it
        // a network error (WPT request-cache-only-if-cached cross-origin).
        if cache_mode == "only-if-cached" && mode == "same-origin" {
            let hit_origin = url::Url::parse(&hit.final_url)
                .ok()
                .map(|u| u.origin().ascii_serialization());
            let same = match (&client_origin, &hit_origin) {
                (Some(c), Some(h)) => c == h,
                _ => false,
            };
            if !same {
                return throw_transport_error(
                    ctx,
                    TransportError {
                        code: "other",
                        message: "only-if-cached: cached response is cross-origin".into(),
                    },
                );
            }
        }
        let response = build_response_from_transport(
            ctx,
            TransportResponse {
                status: hit.status,
                status_text: "OK".to_string(),
                headers: hit.headers,
                content_type: String::new(),
                content_length: Some(hit.body.len() as u64),
                http_version: "1.1",
                resp: None,
                synthetic_body: Some(hit.body),
            },
            RespType::Basic,
            &url_str,
            false,
            signal_val.clone(),
        )?;
        // `fetch()` returns a promise; resolve it with the cached Response.
        return make_resolved_promise(ctx, response);
    }

    let result_slot = PromiseResultSlot::<Result<TransportResponse, FetchError>>::new();
    // Look up cached validators before issuing the request so the conditional
    // headers ride on the outgoing request and the `cache_driven` flag (copied
    // into the response closure below) reflects *our* revalidation.
    // `no-store`/`reload` never attach revalidation validators (they go to
    // network unconditionally); `default` stale and `no-cache` revalidate.
    let revalidate = match cache_mode.as_str() {
        "no-store" | "reload" => false,
        _ => true,
    };
    // A conditional-header request is treated as no-store: no revalidation
    // validators are added on top of the caller's own conditional headers.
    let cached_validators = if cacheable && revalidate && !has_conditional {
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

    // Compute the outgoing Referer value per the Fetch `referrer`/`referrerPolicy`
    // algorithm. Covers the policies exercised by WPT `referrer.any.js`:
    // `origin-when-cross-origin` and the default (`client`-resolved) policy, plus
    // explicit `referrer` values with their credentials/fragment stripped.
    let effective_referrer = compute_request_referrer(
        &referrer,
        &referrer_policy,
        client_origin.as_deref(),
        client_url.as_deref(),
        &url_str,
    )
    .unwrap_or_default();

    // The probing above intentionally tolerates missing properties; clear any
    // exception it may have left behind so the caller does not observe a stale
    // pending error.
    let _ = ctx.take_exception();

    // Clone the client origin for request-header injection; the original is
    // moved into the response callback closure below.
    let req_origin = client_origin.clone();

    // Same reason: keep a copy of the referrer-policy inputs for the transport,
    // which recomputes the header on each redirect hop.
    let rc_origin = client_origin.clone();
    let rc_url = client_url.clone();
    let rc_referrer = referrer.clone();
    let rc_policy = referrer_policy.clone();

    // Captured into the result closure so an *in-flight* abort (one that fires
    // while the request is on the network) rejects with the signal's live
    // reason — including any custom reason passed to `abort()` — exactly like
    // the already-aborted path below. A JSValue is not `Send`, so it must live
    // in the engine job closure (not the `tokio::spawn` closure that does the
    // network I/O), where it is read only after the abort has committed.
    let signal_for_reason = signal_val.clone();

    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), move |cx, data| -> JSResult {
            let client_origin = client_origin.clone();
            let mut tr = match data {
                Ok(tr) => tr,
                Err(e) => {
                    return match e {
                        FetchError::Abort => {
                            // Reject with the signal's live abort reason — a
                            // custom reason passed to `abort()` must surface even
                            // when the abort fires mid-flight (while the request
                            // is on the network). Fall back to the default
                            // "AbortError" DOMException if no signal is present.
                            let reason = signal_for_reason
                                .as_ref()
                                .and_then(|s| signal_reason(s))
                                .unwrap_or_else(|| {
                                    make_dom_exception(
                                        cx,
                                        "AbortError",
                                        "The operation was aborted.",
                                    )
                                    .unwrap_or(JSValue::UNDEFINED)
                                });
                            cx.throw(Exception::Error(reason));
                            Err(-1)
                        }
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

            // Fetch spec: `mode: "same-origin"` requires the request (and every
            // redirect hop) to stay same-origin; a cross-origin URL is a network
            // error. reqwest auto-follows redirects, so we check the *final*
            // response URL (redirect target otherwise hides the violation).
            if mode_clone == "same-origin" {
                let final_url = tr
                    .resp
                    .as_ref()
                    .map(|r| r.url().to_string())
                    .unwrap_or_else(|| url_clone.clone());
                // `data:` URLs are same-origin with any client per the Fetch
                // scheme fetch algorithm, so they never trip the same-origin
                // check regardless of the client origin.
                let is_data = final_url.to_ascii_lowercase().starts_with("data:");
                if !is_data {
                    let req_origin = url::Url::parse(&final_url)
                        .map(|u| u.origin().ascii_serialization())
                        .unwrap_or_default();
                    let same = client_origin.as_deref() == Some(req_origin.as_str());
                    if !same {
                        return throw_transport_error(
                            cx,
                            TransportError {
                                code: "same-origin",
                                message: format!(
                                    "request failed: mode 'same-origin' requires a same-origin \
                                     request, but URL is cross-origin: {final_url}"
                                ),
                            },
                        );
                    }
                }
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
                    signal_val.clone(),
                );
            }
            // Caching a successful GET response (with its full body) is done in
            // the transport closure above, where the body bytes are available.
            // (Skipping `no-store` there too.) Nothing to do here.
            //
            // A `no-cors` request for a cross-origin (or unknown-origin) URL
            // yields an opaque response per Fetch spec (status 0, type opaque,
            // null body, CORS-filtered headers). Same-origin stays basic.
            let is_opaque = mode_clone == "no-cors"
                && is_cross_origin(client_origin.as_deref(), &url_clone);
            let resp_type = if is_opaque {
                RespType::Opaque
            } else {
                RespType::Basic
            };
            // Cross-origin `cors` mode: run the CORS check against the final
            // response origin and, on success, return a `cors` filtered
            // response (type `cors`, headers limited to the safelist +
            // `Access-Control-Expose-Headers`). A failed check is a network
            // error (rejects with TypeError).
            if !is_opaque && cors_tainted {
                let final_url = tr
                    .resp
                    .as_ref()
                    .map(|r| r.url().to_string())
                    .unwrap_or_else(|| url_clone.clone());
                let final_cross = is_cross_origin(client_origin.as_deref(), &final_url);
                if final_cross {
                    let headers = HeaderList::from_vec(tr.headers.clone());
                    let client_origin_str = serialized_origin(client_origin.as_deref());
                    if cors_check(&headers, &client_origin_str, credentials_include).is_none() {
                        return throw_transport_error(
                            cx,
                            TransportError {
                                code: "cors",
                                message: "request failed: CORS check failed for the response"
                                    .into(),
                            },
                        );
                    }
                    // CORS-filter the exposed headers. `content-type` /
                    // `content-length` are safelisted, so the payload's derived
                    // fields stay valid.
                    let expose = headers.get("Access-Control-Expose-Headers");
                    let filtered =
                        cors_filter_response_headers(&headers, expose.as_deref(), credentials_include);
                    tr.headers = filtered;
                    return build_response_from_transport(
                        cx,
                        tr,
                        RespType::Cors,
                        &final_url,
                        false,
                        signal_val.clone(),
                    );
                }
            }
            build_response_from_transport(cx, tr, resp_type, &url_clone, false, signal_val.clone())
        })?;

    if already_aborted {
        // An already-aborted signal must reject on a microtask, *before* any
        // `Promise.resolve().then` callback (WPT "already aborted signal rejects
        // immediately"). Skip the thread/event-loop hop and reject synchronously
        // as a (microtask) job via the promise machinery.
        // Consuming a Request whose signal is already aborted marks its body as
        // used (Fetch spec): `request.bodyUsed` must become true.
        if input.as_native_object::<JSRequest>().is_some() {
            if let Ok(p) = get_request_payload_mut(ctx, &mut input.clone()) {
                p.body_consumed = true;
            }
        }

        let rejected = make_rejected_promise(ctx, abort_reason)?;
        return Ok(rejected.into());
    }

    // Per-host TLS escape hatch: skip cert validation only for hosts listed in
    // AIKE_TLS_INSECURE_HOSTS (self-signed dev/WPT servers). All other hosts
    // keep strict validation.
    let insecure = url::Url::parse(&url_str)
        .ok()
        .and_then(|u| u.host_str().map(|h| (h.to_string(), u.port())))
        .map(|(h, p)| crate::http::client::config::tls_insecure_for_host(&h, p))
        .unwrap_or(false);

    // Fetch spec "HTTP defaults": a request carries default `Accept` and
    // `Accept-Language` headers unless the caller overrides them. reqwest's
    // own defaults are wiped by `perform_request` (it sets each header
    // explicitly), so we must supply them here.
    ensure_default_request_headers(&mut header_list);
    // WHATWG: silently drop forbidden request headers the caller tried to set.
    strip_forbidden_request_headers(&mut header_list);
    // Validate header names/values per the Fetch spec. Must run on the JS
    // thread so a violation rejects the returned promise with a `TypeError`
    // (WPT header-values expects `promise_rejects_js(t, TypeError, ...)`).
    validate_header_list_for_fetch(ctx, &header_list)?;

    // `Origin` is a forbidden request header (scripts cannot set it), so it is
    // added here *after* the forbidden-header strip. Per the Fetch spec an
    // `Origin` header is appended when the method is not GET/HEAD, OR when the
    // request is CORS-tainted. The value is the client's origin, or the
    // literal `null` when the client origin is opaque.
    if should_append_origin(&method, cors_tainted) {
        header_list.set("Origin", &serialized_origin(req_origin.as_deref()));
    }

    // `Content-Length` is likewise forbidden for scripts, so it too is derived
    // here from the actual payload. With no body only POST/PUT still report 0
    // (they normally carry one); other methods send no length at all.
    let implies_body = method.eq_ignore_ascii_case("POST") || method.eq_ignore_ascii_case("PUT");

    if let Some(b) = &body_bytes {
        header_list.set("Content-Length", &b.len().to_string());
    } else if implies_body {
        header_list.set("Content-Length", "0");
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

    // Whether a CORS preflight is required (cross-origin `cors` request with a
    // non-safelisted method or header). Computed here so the redirect policy
    // (below) can stop following redirects once a preflight runs.
    let preflight_needed = needs_preflight(&method, &req_headers, cors_tainted);

    // Manually follow redirects when a referrer must be recomputed per hop, or
    // for cors-mode requests (whose per-hop `Origin` header must be recomputed
    // and whose cross-origin redirects make the origin opaque). A preflighted
    // request must NOT follow redirects at all (Fetch spec: any redirect after
    // a preflight is a network error).
    let manual_redirect = redirect == "follow"
        && !preflight_needed
        && (!effective_referrer.is_empty() || cors_tainted || mode == "cors");

    // Per-host TLS escape hatch: `insecure` skips cert validation for this
    // request's host (AIKE_TLS_INSECURE_HOSTS).
    let mut cfg = crate::http::client::config::HttpClientConfig::default();
    cfg.protocol = crate::http::client::config::ProtocolMode::Http1Only;
    // WHATWG `fetch` manages cookies itself via the shared global jar (driven
    // by the `credentials` mode), so disable reqwest's automatic store which
    // would otherwise send cookies unconditionally.
    cfg.cookie_jar = false;
    // Disable reqwest's auto-follow whenever we follow manually OR a preflight
    // runs (in which case redirects must surface, not be followed).
    cfg.follow_redirects = !manual_redirect && !preflight_needed;

    let client = match cfg.build_http_client_ex(insecure) {
        Ok(c) => c,
        Err(e) => {
            return Err(ctx.throw_internal_error(&format!("failed to build HTTP client: {e}")));
        }
    };

    let referrer_ctx = manual_redirect.then(|| ReferrerCtx {
        referrer: rc_referrer,
        policy: rc_policy,
        client_origin: rc_origin,
        client_url: rc_url,
        max_redirects: 10,
    });

    let req = TransportRequest {
        method,
        url: url_str.clone(),
        headers: req_headers,
        body: body_bytes,
        timeout,
        referrer: effective_referrer,
        referrer_ctx,
        replayable: stream_body.is_none(),
        redirect_follow: !preflight_needed,
        cors_mode: mode == "cors",
        cors_tainted,
        cors_client_origin: serialized_origin(req_origin.as_deref()),
        credentials_mode: credentials.clone(),
        use_shared_cookies: true,
    };

    // CORS preflight: a cross-origin `cors`-mode request whose method or
    // headers are not CORS-safelisted must first complete an OPTIONS preflight
    // against the target, else the actual request is never sent. Computed here
    // (JS thread) so a failed preflight rejects with a TypeError. The preflight
    // runs in the async spawn before the actual request.
    // The actual request carries `User-Agent: Mozilla/5.0 (aike)` (injected by
    // `ensure_default_request_headers`); the preflight must send the same value
    // so a server comparing the two (WPT `checkUserAgentHeaderInPreflight`)
    // accepts the actual request.
    let user_agent = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "Mozilla/5.0 (aike)".to_string());
    let (preflight, preflight_key) = if preflight_needed {
            let requested_headers = preflight_request_headers(&req.headers);
            let key = preflight_cache_key(
                &serialized_origin(req_origin.as_deref()),
                &url_str,
                &req.method,
                &requested_headers,
            );
            let pf = Some(PreflightRequest {
                url: url_str.clone(),
                request_method: req.method.clone(),
                request_headers: crate::http::client::cors::join_request_headers(&requested_headers),
                origin: serialized_origin(req_origin.as_deref()),
                user_agent,
                referrer: req.referrer.clone(),
            });
            (pf, Some(key))
        } else {
            (None, None)
        };

    // Issue the request on the tokio side. Taken as a closure because a stream
    // body must first be pumped on the JS thread, which defers this call to a
    // later event-loop turn.
    let rs = result_slot.clone();
    let notify2 = notify;
    let cache_mode_for_spawn = cache_mode.clone();
    let has_conditional_for_spawn = has_conditional;
    // Values the async side needs to run a CORS preflight before the actual
    // request. Cloned out so the original `client_origin` stays owned by the
    // JS-thread promise callback below.
    let preflight_for_spawn = preflight.clone();
    let preflight_key_for_spawn = preflight_key.clone();
    let cors_client_origin = serialized_origin(req_origin.as_deref());
    let cors_credentials_include = credentials_include;
    let cors_method_for_spawn = req.method.clone();
    let spawn_request = move |req: TransportRequest| match notify2 {
        Some(n) => {
            let preflight = preflight_for_spawn.clone();
            let preflight_key = preflight_key_for_spawn.clone();
            let cors_client_origin = cors_client_origin.clone();
            let cors_credentials_include = cors_credentials_include;
            let cors_method = cors_method_for_spawn.clone();
            tokio::spawn(async move {
                // Run the CORS preflight first; a failed preflight rejects the
                // fetch (and the actual request is never sent). A valid cached
                // preflight skips the network round-trip.
                if let (Some(pf), Some(key)) = (&preflight, &preflight_key) {
                    if !run_preflight(
                        &client,
                        pf,
                        key,
                        &cors_client_origin,
                        cors_credentials_include,
                        &cors_method,
                    )
                    .await
                    {
                        let _ = rs.commit(Err(FetchError::Other(TransportError {
                            code: "cors",
                            message: "request failed: CORS preflight did not succeed".into(),
                        })));
                        return;
                    }
                }
                let fut = perform_request(&client, req);
                tokio::select! {
                    r = fut => {
                        // A preflighted request must not follow a redirect: any
                        // redirect response after a preflight is a network error.
                        let r = r.and_then(|tr| {
                            if is_redirect_status(tr.status) {
                                Err(TransportError {
                                    code: "cors",
                                    message: "request failed: redirect after CORS preflight".into(),
                                })
                            } else {
                                Ok(tr)
                            }
                        });
                        let _ = rs.commit(r.map_err(FetchError::Other));
                    }
                    _ = n.notified() => { let _ = rs.commit(Err(FetchError::Abort)); }
                }
            });
        }
        None => {
            let cache_mode = cache_mode_for_spawn.clone();
            let has_conditional = has_conditional_for_spawn;
            let cache_method = req.method.clone();
            let cache_url = req.url.clone();
            let preflight = preflight_for_spawn.clone();
            let preflight_key = preflight_key_for_spawn.clone();
            let cors_client_origin = cors_client_origin.clone();
            let cors_credentials_include = cors_credentials_include;
            let cors_method = cors_method_for_spawn.clone();
            tokio::spawn(async move {
                // CORS preflight before the actual request (see `Some` branch).
                if let (Some(pf), Some(key)) = (&preflight, &preflight_key) {
                    if !run_preflight(
                        &client,
                        pf,
                        key,
                        &cors_client_origin,
                        cors_credentials_include,
                        &cors_method,
                    )
                    .await
                    {
                        let _ = rs.commit(Err(FetchError::Other(TransportError {
                            code: "cors",
                            message: "request failed: CORS preflight did not succeed".into(),
                        })));
                        return;
                    }
                }
                let mut tr = match perform_request(&client, req).await {
                    Ok(tr) => tr,
                    Err(e) => {
                        let _ = rs.commit(Err(FetchError::Other(e)));
                        return;
                    }
                };
                // A preflighted request must not follow a redirect: any redirect
                // response after a preflight is a network error (Fetch spec).
                if preflight.is_some() && is_redirect_status(tr.status) {
                    let _ = rs.commit(Err(FetchError::Other(TransportError {
                        code: "cors",
                        message: "request failed: redirect after CORS preflight".into(),
                    })));
                    return;
                }
                // Subresource Integrity: for a non-empty `integrity`, the whole
                // body must be read and its hash matched against the metadata,
                // otherwise the response is rejected (fetch spec §6.4).
                if !integrity.is_empty() {
                    let bytes = match tr.resp.take() {
                        Some(resp) => match resp.bytes().await {
                            Ok(b) => b.to_vec(),
                            Err(e) => {
                                let _ = rs.commit(Err(FetchError::Other(TransportError {
                                    code: "integrity",
                                    message: format!(
                                        "request failed: reading body for integrity check: {e}"
                                    ),
                                })));
                                return;
                            }
                        },
                        None => tr.synthetic_body.clone().unwrap_or_default(),
                    };
                    if let Err(msg) = check_integrity(&bytes, &integrity) {
                        let _ = rs.commit(Err(FetchError::Other(TransportError {
                            code: "integrity",
                            message: format!("request failed: {msg}"),
                        })));
                        return;
                    }
                    tr.synthetic_body = Some(bytes);
                }
                // HTTP cache: buffer the body of a cacheable GET response so it
                // can be stored and later served/revalidated from cache. The
                // stored body is also put back as the response's synthetic body.
                if cache_mode != "no-store"
                    && !has_conditional
                    && cache_method.eq_ignore_ascii_case("GET")
                    && (200..300).contains(&tr.status)
                {
                    // The final URL (after any redirect) is stored so a later
                    // `same-origin` cache hit can reject a cross-origin redirect.
                    let final_url = tr
                        .resp
                        .as_ref()
                        .map(|r| r.url().to_string())
                        .unwrap_or_else(|| cache_url.clone());
                    let bytes = match tr.resp.take() {
                        Some(resp) => match resp.bytes().await {
                            Ok(b) => b.to_vec(),
                            Err(_) => Vec::new(),
                        },
                        None => tr.synthetic_body.clone().unwrap_or_default(),
                    };
                    http_cache::store(
                        &cache_method,
                        &cache_url,
                        tr.status,
                        &tr.headers,
                        bytes.clone(),
                        &final_url,
                    );
                    tr.synthetic_body = Some(bytes);
                }

                let _ = rs.commit(Ok(tr));
            });
        }
    };

    // A buffered body can go out immediately; a stream body must be pumped to
    // completion on the JS thread first, then the request is issued with the
    // collected bytes (and the now-known Content-Length).
    match stream_body {
        None => spawn_request(req),
        Some(sv) => {
            let Some(stream) = sv.as_native_object::<JSReadableStream>() else {
                return Err(ctx.throw_type_error("body is not a ReadableStream"));
            };

            let drain_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
            let raw_err = Rc::new(RefCell::new(None::<JSValue>));
            drain_readable_stream(ctx, stream, drain_slot.clone(), raw_err.clone())?;

            // The job may run over several turns, so the request and the
            // one-shot dispatcher are parked in cells and taken on the turn
            // that actually dispatches.
            let req = RefCell::new(Some(req));
            let spawn_request = RefCell::new(Some(spawn_request));
            let fail_slot = result_slot.clone();

            ctx.queue_job(
                move |_cx| {
                    let Some(bytes) = drain_slot.try_take() else {
                        return Ok(JobState::Again);
                    };

                    match bytes {
                        Ok(b) => {
                            let mut req = req.take().expect("stream drain job ran twice");
                            set_content_length(&mut req.headers, b.len());
                            req.body = Some(b);

                            let spawn = spawn_request.take().expect("dispatcher already used");
                            spawn(req);
                        }
                        // The stream errored or produced a bad chunk; surface it
                        // as a network error so the fetch promise rejects.
                        Err(msg) => {
                            let _ = fail_slot.commit(Err(FetchError::Other(TransportError {
                                code: "body",
                                message: format!("request failed: {msg}"),
                            })));
                        }
                    }

                    Ok(JobState::Done)
                },
                (),
            );
        }
    }

    Ok(promise.into())
}

/// Whether an HTTP status is a redirect status (301/302/303/307/308).
fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Run a CORS preflight against the target and validate the response.
///
/// Returns `Some(max_age)` (seconds) on success, or `None` on any failure
/// (redirect, non-2xx status, or a failed `Access-Control-Allow-*` check). The
/// actual request is only sent when the preflight succeeds. A successful
/// preflight whose `Access-Control-Max-Age` is non-zero is stored in the cache.
async fn run_preflight(
    client: &reqwest::Client,
    pf: &PreflightRequest,
    cache_key: &str,
    client_origin: &str,
    credentials_include: bool,
    method: &str,
) -> bool {
    // A still-valid cached preflight (same origin/URL/method/headers) lets us
    // skip the network round-trip (Fetch spec preflight cache).
    if preflight_cache_hit(cache_key) {
        return true;
    }

    let requested_headers: Vec<String> = pf
        .request_headers
        .split(',')
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let tr = match perform_preflight(client, pf).await {
        Ok(tr) => tr,
        Err(_) => return false,
    };
    let headers = HeaderList::from_vec(tr.headers);
    if !check_preflight(
        tr.status,
        &headers,
        client_origin,
        credentials_include,
        method,
        &requested_headers,
    ) {
        return false;
    }
    // Cache the successful preflight per `Access-Control-Max-Age`.
    let max_age = headers
        .get("Access-Control-Max-Age")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    preflight_cache_store(cache_key, max_age);
    true
}

/// Overwrite `Content-Length` on an already-built header list. Used once a
/// stream body has been pumped and its real length is known.
fn set_content_length(headers: &mut Vec<(String, String)>, len: usize) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
    headers.push(("Content-Length".into(), len.to_string()));
}

/// Install the `fetch` global and register all WHATWG classes.
pub(crate) fn install_fetch(ctx: &mut ScopedContext, mut global: Object) -> Result<(), i32> {
    let func = ctx.create_native_function(Callback::Func(js_fetch), "fetch", 1, 0)?;
    global.define_property_value_str(ctx, "fetch", func.into(), zuqe::JS_PROP_C_W_E)?;

    Ok(())
}
