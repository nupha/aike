// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! WHATWG `fetch()`.
//!
//! Split by functionality: the request pipeline (`request`), request-header
//! policy (`headers`), the referrer algorithm (`referrer`) and Subresource
//! Integrity (`integrity`). `cors` holds the browser-security layer (the
//! WHATWG CORS protocol + mode/tainting decisions) and `origin` the client
//! ("document") origin resolution; both are gated by the `fetch_cors`
//! feature — server builds compile them down to constant pass-through stubs.

mod cors;
mod headers;
mod integrity;
mod origin;
mod referrer;
mod request;

pub(crate) use {referrer::compute_request_referrer, request::install_fetch};
