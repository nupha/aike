// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! CDP Network domain helpers — request/response body, cookie.

use tokio::sync::oneshot;

use crate::browser::cdp::CdpClient;

/// Get the HTTP response body for a given request ID.
/// Returns `{ body, base64Encoded }`.
pub(crate) fn get_response_body(
    client: &CdpClient,
    session_id: &str,
    request_id: &str,
) -> oneshot::Receiver<Result<serde_json::Value, String>> {
    client.send(
        "Network.getResponseBody",
        Some(serde_json::json!({ "requestId": request_id })),
        Some(session_id),
    )
}

/// Enable the Network domain (prerequisite for network events).
pub(crate) fn enable_network(
    client: &CdpClient,
    session_id: &str,
) -> oneshot::Receiver<Result<serde_json::Value, String>> {
    client.send("Network.enable", None, Some(session_id))
}

/// Get all browser cookies.
pub(crate) fn get_all_cookies(
    client: &CdpClient,
    session_id: &str,
) -> oneshot::Receiver<Result<serde_json::Value, String>> {
    client.send("Network.getAllCookies", None, Some(session_id))
}

/// Set a single cookie. Params: name, value, url/domain/path, httpOnly, secure, etc.
pub(crate) fn set_cookie(
    client: &CdpClient,
    session_id: &str,
    params: serde_json::Value,
) -> oneshot::Receiver<Result<serde_json::Value, String>> {
    client.send("Network.setCookie", Some(params), Some(session_id))
}
