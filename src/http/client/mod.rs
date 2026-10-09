// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

pub(crate) mod body;
// `cache` stays compiled in both configurations (it owns the `CacheHit` type
// referenced by the fetch layer); every *use* of it is gated on `fetch_cache`.
pub(crate) mod cache;
// The Cache Storage API surface, only exposed with the `fetch_cache` feature.
#[cfg(feature = "fetch_cache")]
pub(crate) mod cache_storage;
mod class;
pub(crate) mod config;
pub(crate) mod headers;
pub(crate) mod request;
pub(crate) mod response;
pub(crate) mod transport;

pub(crate) use class::JSHttpClient;
