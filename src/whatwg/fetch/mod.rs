// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG `fetch()`.
//!
//! Split by functionality: the request pipeline (`request`), request-header
//! policy (`headers`), the referrer algorithm (`referrer`) and Subresource
//! Integrity (`integrity`).

mod headers;
mod integrity;
mod referrer;
mod request;

pub(crate) use {
    referrer::compute_request_referrer,
    request::{gc_trace_fetch, install_fetch},
};
