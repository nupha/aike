// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

use super::headers::HeaderList;

pub(crate) use crate::http::client::transport::{
    TransportRequest, TransportResponse, perform_request,
};

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
        referrer: String::from("about:client"),
    }
}
