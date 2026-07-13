// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Shared HTTP transport for `request()` and `fetch()`.
//!
//! Both the legacy `request()` global and the WHATWG `fetch()` delegate to
//! `perform_request`, which performs the reqwest send and returns the
//! response metadata plus the still-open `reqwest::Response` (body is
//! streamed lazily by the `Response` class).

use std::time::Duration;

use crate::whatwg::fetch::headers::HeaderList;

/// Request passed to [`perform_request`].
pub(crate) struct TransportRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    /// Redirect handling mode: "follow" | "error" | "manual".
    pub redirect: String,

    // --- RequestInit fields (forwarded; some mapped to reqwest behavior) ---
    /// "omit" | "same-origin" | "include". "include"/"same-origin" enable a
    /// per-request cookie store.
    pub credentials: String,
    /// Referrer URL; "about:client" (default) means no Referer header.
    pub referrer: String,
    /// Subresource integrity metadata (stored; SRI validation not enforced).
    pub integrity: String,
    /// "cors" | "no-cors" | "same-origin" | "navigate". Informational here.
    pub mode: String,
    /// "default" | "no-store" | "reload" | "no-cache" | "force-cache" | "only-if-cached".
    pub cache: String,
    /// "no-referrer" | "no-referrer-when-downgrade" | "same-origin" | ...
    pub referrer_policy: String,
    /// Per-request keepalive (connection reuse is inherent in reqwest).
    pub keepalive: bool,
}

/// Result of [`perform_request`]. `resp` is kept open for lazy body streaming.
pub(crate) struct TransportResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub content_type: String,
    pub resp: Option<reqwest::Response>,
}

/// Perform the HTTP request and return its metadata + open response.
pub(crate) async fn perform_request(req: TransportRequest) -> Result<TransportResponse, String> {
    let redirect_policy = match req.redirect.as_str() {
        "error" | "manual" => reqwest::redirect::Policy::none(),
        _ => reqwest::redirect::Policy::limited(10),
    };
    let mut builder = reqwest::Client::builder().redirect(redirect_policy);

    // credentials: "include"/"same-origin" enable a cookie jar.
    if req.credentials == "include" || req.credentials == "same-origin" {
        builder = builder.cookie_store(true);
    }

    let client = builder.build().map_err(|e| e.to_string())?;
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut rb = client.request(method, &req.url).timeout(req.timeout);

    // referrer -> Referer header (unless the caller set one explicitly).
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

    let resp = rb.send().await.map_err(|e| e.to_string())?;
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

/// Helper used by callers that already hold a `HeaderList`.
pub(crate) fn request_with_headers(
    method: String,
    url: String,
    headers: HeaderList,
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> TransportRequest {
    TransportRequest {
        method,
        url,
        headers: headers.to_vec(),
        body,
        timeout,
        redirect: String::from("follow"),
        credentials: String::from("same-origin"),
        referrer: String::from("about:client"),
        integrity: String::new(),
        mode: String::from("cors"),
        cache: String::from("default"),
        referrer_policy: String::new(),
        keepalive: false,
    }
}
