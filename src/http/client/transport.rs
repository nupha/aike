// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Core transport types and the shared `perform_request` function.
//!
//! Both the WHATWG `fetch()` and the `HttpClient` class delegate to
//! `perform_request`, which sends the request via a pre-built
//! `reqwest::Client` and returns the response metadata plus the still-open
//! `reqwest::Response` (body is streamed lazily by the JS `Response` class).

use std::time::Duration;

/// Per-request data passed to [`perform_request`].
///
/// Contains only request-level fields.  Client-level settings (proxy, TLS,
/// cookie jar, …) belong in [`HttpClientConfig`](super::config::HttpClientConfig)
/// and are baked into the `reqwest::Client` at construction time.
pub(crate) struct TransportRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    /// "follow" | "error" | "manual"
    pub redirect: String,
    /// Referrer URL; "about:client" means no Referer header.
    pub referrer: String,
}

/// Result of [`perform_request`]. `resp` is kept open for lazy body streaming.
pub(crate) struct TransportResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub content_type: String,
    pub resp: Option<reqwest::Response>,
}

/// Perform an HTTP request on a pre-built `reqwest::Client`.
///
/// The caller is responsible for building the client with the desired
/// proxy / TLS / cookie / redirect settings via [`HttpClientConfig`].
pub(crate) async fn perform_request(
    client: &reqwest::Client,
    req: TransportRequest,
) -> Result<TransportResponse, String> {
    let method =
        reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
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

    let resp = rb.send().await.map_err(|e| format!("request failed: {e}"))?;
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

    Ok(TransportResponse {
        status: status.as_u16(),
        status_text,
        headers,
        content_type,
        resp: Some(resp),
    })
}
