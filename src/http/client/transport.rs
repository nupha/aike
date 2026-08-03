// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

use base64::Engine;
use zuqe::{ErrorKind, Exception, JSResult, JSValue, ScopedContext};

/// Per-request data passed to [`perform_request`].
///
/// Contains only request-level fields.  Client-level settings (proxy, TLS,
/// cookie jar, redirect policy, …) belong in
/// [`HttpClientConfig`](super::config::HttpClientConfig) and are baked into
/// the `reqwest::Client` at construction time.
pub(crate) struct TransportRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    /// Referrer URL for the initial hop; "about:client" means no Referer header.
    pub referrer: String,
    /// Inputs needed to recompute the referrer on each redirect hop. `None`
    /// disables manual redirect following (the client's own policy applies).
    pub referrer_ctx: Option<ReferrerCtx>,
    /// Whether the body may be sent a second time. False for a stream body,
    /// which the first attempt consumes, disabling the 421 retry.
    pub replayable: bool,
}

/// Policy inputs carried across redirects so each hop can recompute its own
/// `Referer` value. Without this the initial header would be replayed verbatim,
/// which breaks `origin-when-cross-origin` when a hop crosses origins.
pub(crate) struct ReferrerCtx {
    /// The request's `referrer` field ("about:client" or an explicit URL).
    pub referrer: String,
    pub policy: String,
    pub client_origin: Option<String>,
    pub client_url: Option<String>,
    /// Maximum hops to follow before reporting a redirect error.
    pub max_redirects: usize,
}

/// Result of [`perform_request`]. `resp` is kept open for lazy body streaming.
pub(crate) struct TransportResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub content_type: String,
    pub content_length: Option<u64>,
    /// Negotiated HTTP version: "0.9" | "1.0" | "1.1" | "2" | "3".
    pub http_version: &'static str,
    pub resp: Option<reqwest::Response>,
    /// Pre-decoded body for non-network responses (e.g. `data:` URLs).
    pub synthetic_body: Option<Vec<u8>>,
}

/// Decode a `data:` URL (RFC 2397) into body bytes and a media type.
/// Returns `(content_type, body)`. Decoding failures produce an error
/// description string rather than panicking.
fn decode_data_url(url: &str) -> Result<(String, Vec<u8>), String> {
    // Strip the "data:" prefix (already validated by caller).
    let rest = &url["data:".len()..];
    // Split mediatype from data at the first comma.
    let (meta, data) = match rest.find(',') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => return Err("data: URL has no comma separator".into()),
    };
    let is_base64 = meta.to_ascii_lowercase().ends_with(";base64");
    let media_type = if is_base64 {
        &meta[..meta.len() - ";base64".len()]
    } else {
        meta
    };
    let content_type = if media_type.is_empty() {
        "text/plain;charset=US-ASCII".to_string()
    } else {
        media_type.to_string()
    };
    // Payload is percent-decoded unless base64.
    let bytes = if is_base64 {
        let trimmed: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .map_err(|e| format!("invalid base64 in data: URL: {e}"))?
    } else {
        percent_decode(data.as_bytes())
    };
    Ok((content_type, bytes))
}

/// Fetch spec: multiple identical `Content-Length` response headers combine into
/// a single header; conflicting values are all dropped (treated as absent).
fn combine_content_length_headers(raw: Vec<(String, String)>) -> Vec<(String, String)> {
    let values: Vec<&str> = raw
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.as_str())
        .collect();
    if values.len() <= 1 {
        return raw;
    }
    if values.iter().all(|v| *v == values[0]) {
        let mut out = Vec::with_capacity(raw.len());
        let mut merged = false;
        for (k, v) in raw {
            if k.eq_ignore_ascii_case("content-length") {
                if merged {
                    continue;
                }
                out.push((k, v));
                merged = true;
            } else {
                out.push((k, v));
            }
        }
        out
    } else {
        raw.into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
            .collect()
    }
}

/// Percent-decode a `data:`-URL payload (so the body carries the raw octets,
/// e.g. `%EF%BB%BF` -> UTF-8 BOM bytes). Not a full URI decode: only the
/// escapes matter here, and `+` is not special in this context.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'%' if i + 2 < input.len() => {
                let hi = (input[i + 1] as char).to_digit(16);
                let lo = (input[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(input[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Transport failure with a machine-readable category.
///
/// `code` is one of: "timeout" | "connect" | "dns" | "tls" | "redirect" | "other".
pub(crate) struct TransportError {
    pub code: &'static str,
    pub message: String,
}

/// Classify a reqwest error into a short error code for JS-side dispatch.
fn classify(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        return "timeout";
    }
    if e.is_redirect() {
        return "redirect";
    }
    // DNS / TLS causes are buried in the source chain; sniff before the
    // generic connect check (a TLS handshake failure is also is_connect).
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let msg = s.to_string().to_lowercase();
        if msg.contains("dns") {
            return "dns";
        }
        if msg.contains("certificate") || msg.contains("tls") || msg.contains("handshake") {
            return "tls";
        }
        src = s.source();
    }
    if e.is_connect() {
        return "connect";
    }
    "other"
}

/// Encode a header value as latin-1 bytes.
///
/// Fetch validates header values per byte (0x00-0xFF), so a value carrying
/// e.g. `æ` means the byte 0xE6, not its two-byte UTF-8 encoding. Returns
/// `None` when the value is plain ASCII (the common case, where the default
/// `&str` conversion already does the right thing) or when it contains a
/// non-latin-1 char that cannot be represented.
fn latin1_header_value(v: &str) -> Option<reqwest::header::HeaderValue> {
    if v.is_ascii() {
        return None;
    }
    let mut bytes = Vec::with_capacity(v.len());
    for c in v.chars() {
        let cp = c as u32;
        if cp > 0xFF {
            return None;
        }
        bytes.push(cp as u8);
    }
    reqwest::header::HeaderValue::from_bytes(&bytes).ok()
}

/// Map `reqwest::Version` to a short version string.
fn version_str(v: reqwest::Version) -> &'static str {
    match v {
        reqwest::Version::HTTP_09 => "0.9",
        reqwest::Version::HTTP_10 => "1.0",
        reqwest::Version::HTTP_11 => "1.1",
        reqwest::Version::HTTP_2 => "2",
        reqwest::Version::HTTP_3 => "3",
        _ => "",
    }
}

/// Throw a JS `Error` carrying the transport error `code` property, so JS
/// can dispatch on it (e.g. retry on "timeout" but not on "tls").
pub(crate) fn throw_transport_error(cx: &mut ScopedContext, te: TransportError) -> JSResult {
    let mut err = match cx.create_error_object(ErrorKind::Type, &te.message, false) {
        Some(e) => e,
        None => return Err(-1),
    };
    let code = cx.create_string_from_utf8(te.code.as_bytes().to_vec())?;
    err.define_property_value_str(
        cx,
        "code",
        JSValue::make_string(code),
        zuqe::JS_PROP_WRITABLE | zuqe::JS_PROP_CONFIGURABLE,
    )?;
    cx.throw(Exception::Error(err.into()));
    Err(-1)
}

/// Follow redirects by hand, recomputing the `Referer` for every hop.
///
/// The client's own redirect policy replays the initial request headers on
/// each hop, which is wrong for `origin-when-cross-origin`: once a hop crosses
/// origins the referrer must collapse to the origin. So the caller disables
/// auto-follow and we re-issue each hop with a freshly computed referrer.
async fn follow_redirects<F, Fut>(
    rc: &ReferrerCtx,
    start_url: &str,
    start_referrer: &str,
    send: F,
) -> Result<reqwest::Response, TransportError>
where
    F: Fn(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let mut url = start_url.to_string();
    let mut referrer = start_referrer.to_string();

    for _ in 0..=rc.max_redirects {
        let resp = send(url.clone(), referrer.clone())
            .await
            .map_err(|e| TransportError {
                code: classify(&e),
                message: format!("request failed: {e}"),
            })?;

        // A redirect without a usable `Location` is delivered as-is.
        let next = match redirect_target(&resp, &url) {
            Some(u) => u,
            None => return Ok(resp),
        };

        // Recompute the referrer against the *next* URL so a hop that changes
        // origin gets the origin-only form.
        referrer = crate::whatwg::fetch::compute_request_referrer(
            &rc.referrer,
            &rc.policy,
            rc.client_origin.as_deref(),
            rc.client_url.as_deref(),
            &next,
        )
        .unwrap_or_default();
        url = next;
    }

    Err(TransportError {
        code: "redirect",
        message: format!("too many redirects (limit {})", rc.max_redirects),
    })
}

/// Resolve a redirect response's `Location` against the current URL.
/// Returns `None` when the response is not a redirect or has no valid target.
fn redirect_target(resp: &reqwest::Response, current: &str) -> Option<String> {
    if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }

    let loc = resp
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()?;
    let base = url::Url::parse(current).ok()?;
    base.join(loc).ok().map(|u| u.to_string())
}

/// Perform an HTTP request on a pre-built `reqwest::Client`.
///
/// The caller is responsible for building the client with the desired
/// proxy / TLS / cookie / redirect settings via [`HttpClientConfig`].
pub(crate) async fn perform_request(
    client: &reqwest::Client,
    req: TransportRequest,
) -> Result<TransportResponse, TransportError> {
    // `data:` URLs are not network requests — decode them directly.
    if req.url.to_ascii_lowercase().starts_with("data:") {
        let (content_type, body) = decode_data_url(&req.url).map_err(|e| TransportError {
            code: "other",
            message: e,
        })?;
        // HEAD requests carry no body (fetch spec), even for `data:` URLs.
        let is_head = req.method.eq_ignore_ascii_case("HEAD");
        return Ok(TransportResponse {
            status: 200,
            // Non-network (synthetic) responses such as `data:` URLs get the
            // default status 200 with statusText "OK" and type "default", per
            // the WHATWG fetch scheme fetch algorithm. (Network HTTP responses
            // keep statusText "" — the reason phrase is not exposed to scripts.)
            status_text: "OK".to_string(),
            headers: vec![("content-type".into(), content_type.clone())],
            content_type,
            content_length: Some(body.len() as u64),
            http_version: "1.1",
            resp: None,
            synthetic_body: if is_head { None } else { Some(body) },
        });
    }

    // WHATWG fetch: a 421 (Misdirected Request) response means the connection
    // was reused for the wrong origin; retry the request exactly once on a
    // fresh connection. The reqwest client is built with
    // `pool_max_idle_per_host(0)`, so a second attempt opens a clean
    // connection (the server counts it as a new connection).
    // Build one request. Owned args keep the returned future free of borrows,
    // so it can be handed to the redirect loop.
    let send_once = |url: String, referrer: String| {
        let method =
            reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rb = client.request(method, &url).timeout(req.timeout);

        let mut headers = req.headers.clone();
        if referrer != "about:client" && !referrer.is_empty() {
            let has_referer = headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("referer"));
            if !has_referer {
                headers.push(("Referer".to_string(), referrer));
            }
        }

        for (k, v) in &headers {
            match latin1_header_value(v) {
                Some(hv) => rb = rb.header(k, hv),
                None => rb = rb.header(k, v),
            }
        }
        if let Some(b) = &req.body {
            rb = rb.body(b.clone());
        }
        async move { rb.send().await }
    };

    // One attempt at `url`, transparently retried once on a 421 — but only for
    // a replayable body. A stream body is consumed by the first attempt, so the
    // spec requires the 421 to be surfaced instead.
    let replayable = req.replayable;
    let send_with_retry = move |url: String, referrer: String| {
        let first = send_once(url.clone(), referrer.clone());
        async move {
            match first.await {
                Ok(r) if replayable && r.status() == reqwest::StatusCode::MISDIRECTED_REQUEST => {
                    log::debug!("421 on {url}; retrying once on a fresh connection");
                    send_once(url, referrer).await
                }
                other => other,
            }
        }
    };

    let resp = match &req.referrer_ctx {
        Some(rc) => follow_redirects(rc, &req.url, &req.referrer, send_with_retry).await?,
        None => send_with_retry(req.url.clone(), req.referrer.clone())
            .await
            .map_err(|e| TransportError {
                code: classify(&e),
                message: format!("request failed: {e}"),
            })?,
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Decode values as latin-1, not UTF-8: a header value is a byte string and
    // bytes 0x80-0xFF are legal (RFC 7230 obs-text). `to_str()` rejects those,
    // so using it here would silently drop the whole header.
    let raw_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            let vs = v.as_bytes().iter().map(|&b| b as char).collect::<String>();
            (k.to_string(), vs)
        })
        .collect();
    // Fetch spec: multiple identical `Content-Length` headers combine into one;
    // conflicting `Content-Length` values are dropped (treated as absent).
    let headers = combine_content_length_headers(raw_headers);
    // WHATWG fetch: the JS-visible `Response.statusText` is always the empty
    // string; the HTTP reason phrase is intentionally not exposed to scripts.
    let status_text = String::new();
    // Read Content-Length from the header: resp.content_length() reports the
    // body size hint, which is erased by the auto-decompression wrapper.
    let content_length = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut tr = TransportResponse {
        status: status.as_u16(),
        status_text,
        headers,
        content_type,
        content_length,
        http_version: version_str(resp.version()),
        resp: Some(resp),
        synthetic_body: None,
    };
    // WHATWG fetch: 204/205/304 responses and responses to HEAD requests carry
    // no body, so the JS-visible `Response.body` is null and `text()`/etc.
    // resolve to the empty string. Drop the underlying transport so body
    // consumers treat it as a null-body response.
    if req.method.eq_ignore_ascii_case("HEAD") || matches!(tr.status, 204 | 205 | 304) {
        tr.resp = None;
        tr.synthetic_body = None;
    }
    Ok(tr)
}
