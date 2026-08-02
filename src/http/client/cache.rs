// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! A minimal HTTP response cache used to support conditional revalidation
//! (RFC 7234 / Fetch § HTTP-cache). Only enough behaviour is implemented to
//! make conditional GET tests pass:
//!
//! * successful GET responses carrying a validator (`ETag` or
//!   `Last-Modified`) are remembered, keyed by method + URL;
//! * on a later same-URL GET the cache adds `If-None-Match` / `If-Modified-Since`
//!   automatically (unless the caller already supplied them);
//! * a `304 Not Modified` whose validators match a stored entry is turned into
//!   the cached response (status 200) with the 304's headers merged on top, as
//!   the Fetch spec requires.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

/// A cached successful response.
struct CacheEntry {
    /// The cached response status (typically 200).
    status: u16,
    /// Lowercased-name → value header list of the stored response.
    headers: Vec<(String, String)>,
    /// Buffered body — empty for streamed responses we cannot retain.
    body: Vec<u8>,
    /// `ETag` validator, if present.
    etag: Option<String>,
    /// `Last-Modified` validator, if present.
    last_modified: Option<String>,
}

/// Global, process-wide cache. Keyed by (method, url).
static CACHE: LazyLock<Mutex<HashMap<(String, String), CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    headers
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase() == lower)
        .map(|(_, v)| v.clone())
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    header_value(headers, name).is_some()
}

/// True when the caller already supplied a conditional-revalidation header
/// (`If-None-Match` / `If-Modified-Since`). In that case a `304` is the
/// caller's own response and must NOT be turned into the cached response.
pub(crate) fn caller_supplied_validator(headers: &[(String, String)]) -> bool {
    has_header(headers, "if-none-match") || has_header(headers, "if-modified-since")
}

/// Look up a cached entry for a GET request and, if found, return the
/// validators so the caller can attach conditional request headers.
pub(crate) fn lookup_validators(
    method: &str,
    url: &str,
) -> Option<(Option<String>, Option<String>)> {
    if method.to_ascii_uppercase() != "GET" {
        return None;
    }
    let cache = CACHE.lock().unwrap();
    cache
        .get(&(method.to_ascii_uppercase(), url.to_string()))
        .map(|e| (e.etag.clone(), e.last_modified.clone()))
}

/// Append conditional revalidation headers to `headers` from a cached entry,
/// but only for headers the caller did not already provide. Returns `true`
/// when any validator was actually added (i.e. the revalidation is cache-driven
/// and a resulting 304 must be turned into the cached response).
pub(crate) fn add_conditional_headers(
    headers: &mut Vec<(String, String)>,
    validators: (Option<String>, Option<String>),
) -> bool {
    let (etag, last_modified) = validators;
    let mut added = false;
    if let Some(etag) = etag {
        if !has_header(headers, "if-none-match") {
            headers.push(("If-None-Match".to_string(), etag));
            added = true;
        }
    }
    if let Some(lm) = last_modified {
        if !has_header(headers, "if-modified-since") {
            headers.push(("If-Modified-Since".to_string(), lm));
            added = true;
        }
    }
    added
}

/// Store a successful GET response so it can be revalidated later.
pub(crate) fn store(
    method: &str,
    url: &str,
    status: u16,
    headers: &[(String, String)],
    body: Vec<u8>,
) {
    if method.to_ascii_uppercase() != "GET" {
        return;
    }
    // Only cache responses that carry a validator; everything else is not
    // revalidateable and caching it would only add risk of stale data.
    let etag = header_value(headers, "etag");
    let last_modified = header_value(headers, "last-modified");
    if etag.is_none() && last_modified.is_none() {
        return;
    }
    // Never cache `no-store` / `private` responses.
    if let Some(cc) = header_value(headers, "cache-control") {
        let cc = cc.to_ascii_lowercase();
        if cc.contains("no-store") || cc.contains("private") {
            return;
        }
    }
    let mut cache = CACHE.lock().unwrap();
    cache.insert(
        (method.to_ascii_uppercase(), url.to_string()),
        CacheEntry {
            status,
            headers: headers.to_vec(),
            body,
            etag,
            last_modified,
        },
    );
}

/// Merge `update` headers onto a stored entry and rebuild it. Used when the
/// server answers a revalidation with `304`: per Fetch, the 304 response's
/// headers update the stored response but the body stays the same.
fn merge_and_update(method: &str, url: &str, update: &[(String, String)]) {
    let mut cache = CACHE.lock().unwrap();
    if let Some(entry) = cache.get_mut(&(method.to_ascii_uppercase(), url.to_string())) {
        for (k, v) in update {
            if let Some(slot) = entry
                .headers
                .iter_mut()
                .find(|(ek, _)| ek.to_ascii_lowercase() == k.to_ascii_lowercase())
            {
                slot.1 = v.clone();
            } else {
                entry.headers.push((k.clone(), v.clone()));
            }
        }
    }
}

/// If `status == 304` and the cache has a matching entry that we drove the
/// revalidation for (`cache_driven` true), return the cached response's
/// (status, headers, body) with the 304 headers merged on top. Otherwise
/// `None`.
pub(crate) fn take_not_modified(
    method: &str,
    url: &str,
    status: u16,
    not_modified_headers: &[(String, String)],
    cache_driven: bool,
) -> Option<(u16, Vec<(String, String)>, Vec<u8>)> {
    if status != 304 || !cache_driven {
        return None;
    }
    let key = (method.to_ascii_uppercase(), url.to_string());
    let cached_status = {
        let cache = CACHE.lock().unwrap();
        let entry = cache.get(&key)?;
        entry.status
    };
    // Merge 304 headers onto the stored headers.
    merge_and_update(method, url, not_modified_headers);
    let cache = CACHE.lock().unwrap();
    let entry = cache.get(&key)?;
    Some((cached_status, entry.headers.clone(), entry.body.clone()))
}

/// Remove a cache entry (e.g. when a response is unsafe to reuse). Currently
/// unused but kept for symmetry / future invalidation needs.
#[allow(dead_code)]
pub(crate) fn invalidate(method: &str, url: &str) {
    let mut cache = CACHE.lock().unwrap();
    cache.remove(&(method.to_ascii_uppercase(), url.to_string()));
}
