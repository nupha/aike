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
    // `data:` URLs are not network requests — decode them directly.
    if req.url.to_ascii_lowercase().starts_with("data:") {
        let (content_type, body) = decode_data_url(&req.url).map_err(|e| TransportError {
            code: "other",
            message: e,
        })?;
        return Ok(TransportResponse {
            status: 200,
            status_text: "OK".into(),
            headers: vec![("content-type".into(), content_type.clone())],
            content_type,
            content_length: Some(body.len() as u64),
            http_version: "1.1",
            resp: None,
            synthetic_body: Some(body),
        });
    }
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
        synthetic_body: None,
    })
}
