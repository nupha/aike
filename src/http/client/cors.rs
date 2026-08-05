// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! CORS (Cross-Origin Resource Sharing) handling for the Fetch API.
//!
//! Implements the WHATWG Fetch "CORS protocol" pieces the fetch path needs:
//! deciding whether a cross-origin request must be preflighted, validating
//! `Access-Control-Allow-Origin` on responses, and CORS-filtering the response
//! header list. Pure functions; the network side lives in `transport.rs` and
//! the JS-facing response building in `response.rs`.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use super::headers::{HeaderList, is_cors_safelisted};

/// CORS-safelisted response-header names (Fetch spec §5.1.1). These are the
/// only headers exposed on a `cors` filtered response unless the server lists
/// more via `Access-Control-Expose-Headers`.
const CORS_SAFELISTED_RESPONSE_HEADERS: &[&str] = &[
    "cache-control",
    "content-language",
    "content-length",
    "content-type",
    "expires",
    "last-modified",
    "pragma",
];

/// CORS-forbidden response-header names: never exposed on a `cors` response.
const CORS_FORBIDDEN_RESPONSE_HEADERS: &[&str] = &["set-cookie", "set-cookie2"];

/// Serialized origin of the client (the document origin), or the literal
/// `null` when the origin is opaque/unknown. Used for the `Origin` header and
/// `Access-Control-Allow-Origin` comparison.
pub(crate) fn serialized_origin(client_origin: Option<&str>) -> String {
    client_origin
        .map(|s| s.to_string())
        .unwrap_or_else(|| "null".to_string())
}

/// Whether a request URL is cross-origin with respect to the client origin.
/// An unparsable request URL or an unknown client origin counts as
/// cross-origin (conservative: such requests get CORS treatment).
///
/// `data:` URLs are same-origin with any client per the Fetch scheme fetch
/// algorithm (their origin is the client's), so they are never cross-origin.
/// Treating them as cross-origin would wrongly CORS-taint `fetch("data:...")`
/// (default `cors` mode) and fail the response CORS check.
pub(crate) fn is_cross_origin(client_origin: Option<&str>, url: &str) -> bool {
    if url.to_ascii_lowercase().starts_with("data:") {
        return false;
    }
    let req_origin = url::Url::parse(url)
        .ok()
        .map(|u| u.origin().ascii_serialization());
    match (&client_origin, &req_origin) {
        (Some(co), Some(ro)) => co != ro,
        _ => true,
    }
}

/// Whether the `Origin` header should be appended to a request (Fetch spec
/// "append a request `Origin` header"): set when the method is not GET/HEAD,
/// or when the request's response tainting is `cors` (i.e. a cross-origin
/// `cors`-mode request).
pub(crate) fn should_append_origin(method: &str, cors_tainted: bool) -> bool {
    let safe_method = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
    !safe_method || cors_tainted
}

/// CORS-safelisted method (Fetch spec): GET, HEAD, or POST (case-insensitive).
pub(crate) fn cors_safelisted_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "POST"
    )
}

/// Fetch spec "method normalize": uppercase the method if it is a
/// byte-case-insensitive match for one of DELETE, GET, HEAD, OPTIONS, POST, PUT.
/// Any other method (e.g. `patch`, custom verbs) is left as-is.
pub(crate) fn normalize_method(method: &str) -> String {
    match method.to_ascii_uppercase().as_str() {
        "DELETE" | "GET" | "HEAD" | "OPTIONS" | "POST" | "PUT" => method.to_ascii_uppercase(),
        _ => method.to_string(),
    }
}

/// Whether a request-header is CORS-safelisted (name+value).
fn cors_safelisted_request_header(name: &str, value: &str) -> bool {
    is_cors_safelisted(name, value)
}

/// UA-controlled request-header names (Fetch spec). These are set by the
/// browser/UA (or the fetch algorithm itself), not the script, so they never
/// trigger a CORS preflight and are never sent in
/// `Access-Control-Request-Headers`. `Accept`/`Accept-Language`/
/// `Content-Language` are deliberately NOT listed here: a script-set unsafe
/// value on those must still force a preflight (checked via
/// `is_cors_safelisted`).
fn is_ua_controlled_request_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-length" | "origin" | "referer" | "user-agent"
    )
}

/// Whether a request-header would need preflight permission: not CORS-safelisted
/// (name+value) and not UA-controlled.
fn is_cors_unsafe_request_header(name: &str, value: &str) -> bool {
    !cors_safelisted_request_header(name, value) && !is_ua_controlled_request_header(name)
}

/// Whether a cross-origin `cors`-mode request must be preflighted.
///
/// A preflight is required when the method is not CORS-safelisted, or when any
/// request header is CORS-unsafe (non-safelisted and not UA-controlled).
/// Credentials mode on its own does not force a preflight.
pub(crate) fn needs_preflight(
    method: &str,
    headers: &[(String, String)],
    cors_tainted: bool,
) -> bool {
    if !cors_tainted {
        return false;
    }
    if !cors_safelisted_method(method) {
        return true;
    }
    headers
        .iter()
        .any(|(name, value)| is_cors_unsafe_request_header(name, value))
}

/// Validate an `Access-Control-Allow-Origin` header against the client origin.
///
/// Returns the matched origin string (`*` when the wildcard is used) on
/// success, or `None` when the CORS check fails. A `*` only matches when
/// credentials mode is not `include`.
pub(crate) fn check_allow_origin(
    allow_origin: Option<&str>,
    client_origin: &str,
    credentials_include: bool,
) -> Option<String> {
    let ao = allow_origin?.trim();
    if ao == "*" && !credentials_include {
        return Some("*".to_string());
    }
    if ao == client_origin {
        return Some(ao.to_string());
    }
    None
}

/// Validate a CORS response (Fetch spec "CORS-check" algorithm, the part
/// relevant to the `Access-Control-Allow-Origin` header).
///
/// `status` is unused for the non-preflight check but kept for symmetry with
/// `check_preflight`. Returns the matched origin on success.
pub(crate) fn cors_check(
    headers: &HeaderList,
    client_origin: &str,
    credentials_include: bool,
) -> Option<String> {
    let allow_origin = headers.get("Access-Control-Allow-Origin");
    if credentials_include {
        // With credentials, the server must both allow the exact origin and
        // send `Access-Control-Allow-Credentials: true`.
        let allow_credentials = headers.get("Access-Control-Allow-Credentials");
        if allow_credentials.as_deref() != Some("true") {
            return None;
        }
    }
    check_allow_origin(allow_origin.as_deref(), client_origin, credentials_include)
}

/// Split a header value on commas, trimming whitespace around each item
/// (Fetch spec "get, decode, and split"). Empty items are dropped.
pub(crate) fn split_header_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// CORS-filter the response header list for a `cors` filtered response
/// (Fetch spec §5.1.2).
///
/// Keeps: CORS-safelisted response headers, plus any header named in
/// `Access-Control-Expose-Headers` (or everything except the CORS-forbidden
/// set when expose-headers is `*` and credentials mode is not `include`).
/// `Set-Cookie`/`Set-Cookie2` are always dropped.
pub(crate) fn cors_filter_response_headers(
    headers: &HeaderList,
    expose_headers: Option<&str>,
    credentials_include: bool,
) -> Vec<(String, String)> {
    let expose = expose_headers.map(split_header_list).unwrap_or_default();
    let wildcard = !credentials_include && expose.iter().any(|e| e == "*");

    headers
        .entries()
        .into_iter()
        .filter(|(name, _)| {
            let lower = name.to_lowercase();
            if CORS_FORBIDDEN_RESPONSE_HEADERS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(&lower))
            {
                return false;
            }
            if CORS_SAFELISTED_RESPONSE_HEADERS
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&lower))
            {
                return true;
            }
            if wildcard {
                return true;
            }
            expose.iter().any(|e| e.eq_ignore_ascii_case(&lower))
        })
        .collect()
}

/// Validate a preflight response (Fetch spec "CORS-preflight fetch" /
/// "CORS-preflight request" checking).
///
/// Returns `Ok` when the preflight succeeded. A preflight fails on:
/// - a redirect status (301/302/303/307/308) — the preflight is not followed;
/// - any status outside 200–299;
/// - `Access-Control-Allow-Methods` not covering the request method;
/// - `Access-Control-Allow-Headers` not covering every requested header;
/// - a failed `Access-Control-Allow-Origin` check (for credentialed requests
///   the wildcard is not allowed, and `Allow-Credentials` must be true).
///
/// `requested_headers` is the sorted list of non-safelisted request-header
/// names (lowercased) that the preflight must ask permission for.
pub(crate) fn check_preflight(
    status: u16,
    headers: &HeaderList,
    client_origin: &str,
    credentials_include: bool,
    method: &str,
    requested_headers: &[String],
) -> bool {
    // Preflights must not follow redirects.
    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return false;
    }
    if !(200..=299).contains(&status) {
        return false;
    }

    // Allow-Origin check (same rules as the non-preflight CORS check).
    if cors_check(headers, client_origin, credentials_include).is_none() {
        return false;
    }

    // Allow-Methods must be a list of valid method tokens; an invalid token
    // (e.g. one containing a space) fails the whole preflight (Fetch spec:
    // "CORS-preflight fetch" parses it as a header list, and any invalid item
    // is a network error).
    let allow_methods = headers.get("Access-Control-Allow-Methods");
    if let Some(am) = allow_methods {
        let list = split_header_list(&am);
        if !list.iter().all(|m| is_valid_http_token(m)) {
            return false;
        }
        // Method matching is byte-case-insensitive for CORS-safelisted methods
        // (GET/HEAD/POST) and byte-case-sensitive for everything else (Fetch
        // spec; the method-normalize step uppercases the 6 known methods but
        // not custom ones like `patch`).
        let covered = list.iter().any(|m| {
            if m == "*" {
                // The `*` wildcard covers any method only without credentials;
                // but an exact `*` request method matches even with credentials
                // (Fetch spec: character match beats the credentials rule).
                return !credentials_include || method == "*";
            }
            if cors_safelisted_method(method) {
                m.eq_ignore_ascii_case(method)
            } else {
                m == method
            }
        });
        if !covered {
            return false;
        }
    } else if !cors_safelisted_method(method) {
        // A non-safelisted method needs an `Access-Control-Allow-Methods`.
        return false;
    }

    // Allow-Headers must be a list of valid header names; an invalid item (e.g.
    // `Bad value`) fails the whole preflight.
    if !requested_headers.is_empty() {
        let allow_headers = headers.get("Access-Control-Allow-Headers");
        let list = allow_headers
            .map(|v| split_header_list(&v))
            .unwrap_or_default();
        if !list.iter().all(|h| is_valid_header_name(h)) {
            return false;
        }
        let wildcard = list.iter().any(|h| h == "*");
        let covered = requested_headers.iter().all(|rh| {
            let non_wildcard = is_cors_non_wildcard_request_header_name(rh);
            if non_wildcard {
                // `*` does not cover these; must be explicitly listed.
                list.iter().any(|h| h.eq_ignore_ascii_case(rh))
            } else if wildcard && !credentials_include {
                true
            } else {
                list.iter().any(|h| h.eq_ignore_ascii_case(rh))
            }
        });
        if !covered {
            return false;
        }
    }

    true
}

/// Whether `s` is a valid HTTP token (used for header names / methods). A value
/// containing anything other than tchars (e.g. a space) is invalid.
fn is_valid_http_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            matches!(b,
                b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
                | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*'
                | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
}

/// A valid HTTP header name is a non-empty HTTP token.
fn is_valid_header_name(s: &str) -> bool {
    is_valid_http_token(s)
}

/// CORS non-wildcard request-header names (Fetch spec): `*` in
/// `Access-Control-Allow-Headers` does not cover these; they must be listed
/// explicitly. `Sec-*` is also excluded but scripts cannot set those anyway.
fn is_cors_non_wildcard_request_header_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "authorization" || lower == "proxy-authorization" || lower.starts_with("sec-")
}

/// Compute the sorted, comma-joined `Access-Control-Request-Headers` value for
/// a preflight: the lowercased names of all non-CORS-safelisted request headers,
/// sorted, comma-separated (matching what the WPT server echoes back).
pub(crate) fn preflight_request_headers(headers: &[(String, String)]) -> Vec<String> {
    let mut names: Vec<String> = headers
        .iter()
        .filter(|(name, value)| is_cors_unsafe_request_header(name, value))
        .map(|(name, _)| name.to_lowercase())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Build the `Access-Control-Request-Headers` value from a sorted name list.
pub(crate) fn join_request_headers(names: &[String]) -> String {
    names.join(",")
}

// ---------------------------------------------------------------------------
// CORS preflight cache
// ---------------------------------------------------------------------------

struct PreflightCacheEntry {
    expiry_secs: u64,
}

/// Shared in-memory CORS preflight cache (Fetch spec §CORS-preflight fetch,
/// step "If cache contains a CORS-preflight cache entry"). Keyed by the
/// request's origin, URL, method, and requested-header names.
type PreflightCache = Mutex<HashMap<String, PreflightCacheEntry>>;

static PREFLIGHT_CACHE: OnceLock<PreflightCache> = OnceLock::new();

fn preflight_cache() -> &'static PreflightCache {
    PREFLIGHT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build the cache key for a preflight.
pub(crate) fn preflight_cache_key(
    origin: &str,
    url: &str,
    method: &str,
    requested_headers: &[String],
) -> String {
    let mut parts = vec![
        origin.to_string(),
        url.to_string(),
        method.to_ascii_lowercase(),
    ];
    parts.extend(requested_headers.iter().cloned());
    parts.join("\u{0}")
}

/// Whether a valid (unexpired) cached preflight exists for `key`.
pub(crate) fn preflight_cache_hit(key: &str) -> bool {
    let mut cache = preflight_cache().lock().unwrap();
    let now = now_unix_secs();
    match cache.get(key) {
        Some(e) if e.expiry_secs > now => true,
        Some(_) => {
            cache.remove(key);
            false
        }
        None => false,
    }
}

/// Store a successful preflight for `key` with the given `max_age` (seconds).
/// `max_age == 0` disables caching (Fetch spec: 0 or absent → no cache entry).
pub(crate) fn preflight_cache_store(key: &str, max_age: u64) {
    if max_age == 0 {
        return;
    }
    let mut cache = preflight_cache().lock().unwrap();
    cache.insert(
        key.to_string(),
        PreflightCacheEntry {
            expiry_secs: now_unix_secs() + max_age,
        },
    );
}
