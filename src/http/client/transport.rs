// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

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
    /// Referrer URL; "about:client" means no Referer header.
    pub referrer: String,
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

/// Perform an HTTP request on a pre-built `reqwest::Client`.
///
/// The caller is responsible for building the client with the desired
/// proxy / TLS / cookie / redirect settings via [`HttpClientConfig`].
pub(crate) async fn perform_request(
    client: &reqwest::Client,
    req: TransportRequest,
) -> Result<TransportResponse, TransportError> {
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut rb = client.request(method, &req.url).timeout(req.timeout);

    // referrer → Referer header (unless the caller set one explicitly).
    let mut headers = req.headers;
    if req.referrer != "about:client" && !req.referrer.is_empty() {
        let has_referer = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("referer"));
        if !has_referer {
            headers.push(("Referer".to_string(), req.referrer.clone()));
        }
    }

    for (k, v) in &headers {
        rb = rb.header(k, v);
    }
    if let Some(b) = req.body {
        rb = rb.body(b);
    }

    let resp = rb.send().await.map_err(|e| TransportError {
        code: classify(&e),
        message: format!("request failed: {e}"),
    })?;

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|vs| (k.to_string(), vs.to_string())))
        .collect();
    let status_text = status.canonical_reason().unwrap_or("").to_string();
    // Read Content-Length from the header: resp.content_length() reports the
    // body size hint, which is erased by the auto-decompression wrapper.
    let content_length = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    Ok(TransportResponse {
        status: status.as_u16(),
        status_text,
        headers,
        content_type,
        content_length,
        http_version: version_str(resp.version()),
        resp: Some(resp),
    })
}
