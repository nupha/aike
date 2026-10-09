// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! A minimal HTTP response cache (RFC 7234 / Fetch § HTTP-cache).
//!
//! Supports the cache modes exercised by WPT `fetch/api/request/request-cache-*`:
//! `default`, `no-store`, `reload`, `no-cache`, `force-cache`, `only-if-cached`.
//!
//! A successful GET response is remembered keyed by method + URL. Freshness is
//! judged from `Cache-Control: max-age` or `Expires`; a stale entry is
//! revalidated with conditional headers (`If-None-Match` / `If-Modified-Since`),
//! and a `304` is turned back into the cached response with the 304 headers
//! merged on top, as the Fetch spec requires.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// Snapshot of a stored response returned to the caller.
pub(crate) struct CacheHit {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether the entry is fresh per its `max-age`/`Expires` headers.
    pub fresh: bool,
    /// The response's final URL after redirects (the requested URL when no
    /// redirect was followed), used for same-origin checks.
    pub final_url: String,
}

/// A cached successful response.
struct CacheEntry {
    status: u16,
    /// Lowercased-name → value header list of the stored response.
    headers: Vec<(String, String)>,
    /// Buffered body.
    body: Vec<u8>,
    /// `ETag` validator, if present.
    etag: Option<String>,
    /// `Last-Modified` validator, if present.
    last_modified: Option<String>,
    /// When the entry was stored (epoch seconds), for freshness calculation.
    stored_at: u64,
    /// Final response URL after redirects (same as key URL when not redirected).
    final_url: String,
    /// The `Vary` field names (lowercased) the stored response declared.
    vary: Vec<String>,
    /// The request headers this entry was stored for, used to decide whether a
    /// later request matches the `Vary` fields.
    req_headers: Vec<(String, String)>,
}

/// Upper bound on a body we are willing to hold in the in-memory cache.
///
/// The cache is a plain `HashMap<(method,url), Vec<u8>>`; buffering an
/// unbounded body (e.g. the 8GiB file in WPT `huge-fetch`) would OOM the
/// process or yield an empty chunk. Responses larger than this are not cached
/// (the spec does not require caching every response), and the caller is
/// expected to stream such bodies to the consumer instead of buffering them.
pub(crate) const MAX_CACHE_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Global, process-wide cache. Keyed by (method, url).
static CACHE: LazyLock<Mutex<HashMap<(String, String), CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

/// Parse an IMF-fixdate HTTP-date (`Expires`/`Date`) into epoch seconds, or
/// `None` if unparsable. Cache tests emit `Date.toGMTString()`, which is the
/// IMF-fixdate form: `"Fri, 04 Aug 2026 00:00:00 GMT"`.
fn parse_http_date(value: &str) -> Option<u64> {
    let v = value.trim();
    // "ddd, DD Mon YYYY HH:MM:SS GMT"
    let parts: Vec<&str> = v.split_whitespace().collect();
    if parts.len() != 6 || !parts[0].ends_with(',') {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month = match parts[2].to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = parts[3].parse().ok()?;
    let time: Vec<u32> = parts[4]
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    if time.len() != 3 {
        return None;
    }
    let (hh, mm, ss) = (time[0], time[1], time[2]);
    days_from_civil(year, month, day)
        .and_then(|d| d.checked_mul(86400))
        .and_then(|d| d.checked_add((hh * 3600 + mm * 60 + ss) as i64))
        .and_then(|secs| u64::try_from(secs).ok())
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month as i64 + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// True when the stored entry is fresh: `max-age` in `Cache-Control` (or the
/// `Expires` date) is still in the future relative to storage time.
fn is_fresh(entry: &CacheEntry) -> bool {
    if let Some(cc) = header_value(&entry.headers, "cache-control") {
        let cc = cc.to_ascii_lowercase();
        if let Some(pos) = cc.find("max-age=") {
            let rest = &cc[pos + "max-age=".len()..];
            let max_age: u64 = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return now_epoch().saturating_sub(entry.stored_at) < max_age;
        }
    }
    if let Some(expires) = header_value(&entry.headers, "expires")
        && let Some(expires_epoch) = parse_http_date(&expires)
    {
        return now_epoch() < expires_epoch;
    }

    // RFC 7234 §4.2.2 heuristic freshness: with no explicit expiry, allow a small
    // fraction (10%) of the apparent age (time since `Last-Modified`). The
    // response must be shared, i.e. carry `Cache-Control: public` (WPT
    // `http-cache/heuristic`: an unknown response with `public` is reused, the
    // same response without it is not).
    // RFC 7234 §4.2.2 heuristic freshness: with no explicit expiry, allow a small
    // fraction (10%) of the apparent age (time since `Last-Modified`).
    if let Some(lm) = header_value(&entry.headers, "last-modified")
        && let Some(modified) = parse_http_date(&lm)
    {
        let apparent_age = now_epoch().saturating_sub(modified);
        let current_age = now_epoch().saturating_sub(entry.stored_at);
        // Fresh while the entry is younger than 10% of the apparent age.
        return (current_age as f64) < (apparent_age as f64) * 0.1;
    }

    // No freshness info: conservatively treat as stale (requires revalidation).
    false
}

/// Whether a request may reuse an entry given its `Vary` fields: every listed
/// field must be present in both the stored and the current request with the
/// same value (RFC 9111 §4.1). `Vary: *` never matches.
fn vary_matches(entry: &CacheEntry, req_headers: &[(String, String)]) -> bool {
    for field in &entry.vary {
        if field == "*" {
            return false;
        }
        fn pick<'a>(h: &'a [(String, String)], field: &str) -> Option<&'a str> {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(field))
                .map(|(_, v)| v.as_str())
        }
        // A field missing from either side cannot be shown to match.
        match (pick(&entry.req_headers, field), pick(req_headers, field)) {
            (Some(a), Some(b)) if a == b => {}
            _ => return false,
        }
    }
    true
}

/// RFC 9111 §3: the status codes a response may be cached from without an
/// explicit freshness directive. Anything else (e.g. an unknown 299) is only
/// cacheable when it carries `Cache-Control` / `Expires`
/// (WPT `http-cache/heuristic`).
fn is_heuristically_cacheable_status(status: u16) -> bool {
    matches!(
        status,
        200 | 203 | 204 | 300 | 301 | 308 | 404 | 405 | 410 | 414 | 501
    )
}

/// True when the caller supplied any conditional request header
/// (`If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`, `If-Match`,
/// `If-Range`). A request carrying one of these is treated as `no-store`:
/// it bypasses the cache (WPT request-cache-default-conditional) and its
/// response is not stored.
pub(crate) fn caller_supplied_validator(headers: &[(String, String)]) -> bool {
    const CONDITIONAL: &[&str] = &[
        "if-none-match",
        "if-modified-since",
        "if-unmodified-since",
        "if-match",
        "if-range",
    ];
    CONDITIONAL.iter().any(|h| has_header(headers, h))
}

/// Look up a cached entry for a GET request and, if found, return the
/// validators so the caller can attach conditional request headers.
pub(crate) fn lookup_validators(
    method: &str,
    url: &str,
    req_headers: &[(String, String)],
) -> Option<(Option<String>, Option<String>)> {
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    let cache = CACHE.lock().unwrap();
    let entry = cache.get(&(method.to_ascii_uppercase(), url.to_string()))?;
    // A response that varies on fields this request does not match is not
    // revalidateable either (WPT `http-cache/vary`).
    if !vary_matches(entry, req_headers) {
        return None;
    }
    Some((entry.etag.clone(), entry.last_modified.clone()))
}

/// Return the full cached entry for a GET request, if present. Used by the
/// cache modes that can serve straight from cache (`default` fresh,
/// `force-cache`, `only-if-cached`).
pub(crate) fn lookup_full(
    method: &str,
    url: &str,
    req_headers: &[(String, String)],
) -> Option<CacheHit> {
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    let cache = CACHE.lock().unwrap();
    let entry = cache.get(&(method.to_ascii_uppercase(), url.to_string()))?;
    // `Vary: *` can never be reused (it always varies), and a response whose
    // Vary fields do not match this request must not be served either
    // (WPT request-cache-force-cache, http-cache/vary).
    if header_value(&entry.headers, "vary").is_some_and(|v| v.trim() == "*")
        || !vary_matches(entry, req_headers)
    {
        return None;
    }
    Some(CacheHit {
        status: entry.status,
        headers: entry.headers.clone(),
        body: entry.body.clone(),
        fresh: is_fresh(entry),
        final_url: entry.final_url.clone(),
    })
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
    if let Some(etag) = etag
        && !has_header(headers, "if-none-match")
    {
        headers.push(("If-None-Match".to_string(), etag));
        added = true;
    }
    if let Some(lm) = last_modified
        && !has_header(headers, "if-modified-since")
    {
        headers.push(("If-Modified-Since".to_string(), lm));
        added = true;
    }

    added
}

/// Store a successful GET response so it can be reused or revalidated later.
pub(crate) fn store(
    method: &str,
    url: &str,
    status: u16,
    headers: &[(String, String)],
    body: Vec<u8>,
    final_url: &str,
    req_headers: &[(String, String)],
) {
    if !method.eq_ignore_ascii_case("GET") {
        return;
    }
    // Never buffer an unbounded body in the in-memory cache: an 8GiB response
    // would OOM the process. The caller should stream such bodies instead.
    if (body.len() as u64) > MAX_CACHE_BODY_BYTES {
        return;
    }

    // Only cache responses that carry a validator; everything else is not
    // revalidateable and caching it would only add risk of stale data.
    let etag = header_value(headers, "etag");
    let last_modified = header_value(headers, "last-modified");
    if etag.is_none() && last_modified.is_none() {
        return;
    }

    // A status outside the heuristically-cacheable set needs an explicit
    // freshness directive to be cached at all (RFC 9111 §3).
    let cc = header_value(headers, "cache-control")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let explicit_freshness = cc.contains("max-age")
        || cc.contains("s-maxage")
        || cc.contains("public")
        || has_header(headers, "expires");
    if !is_heuristically_cacheable_status(status) && !explicit_freshness {
        return;
    }

    // Never cache `no-store` / `private` responses.
    if let Some(cc) = header_value(headers, "cache-control") {
        let cc = cc.to_ascii_lowercase();
        if cc.contains("no-store") || cc.contains("private") {
            return;
        }
    }
    let vary: Vec<String> = header_value(headers, "vary")
        .map(|v| {
            v.split(',')
                .map(|f| f.trim().to_ascii_lowercase())
                .filter(|f| !f.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut cache = CACHE.lock().unwrap();
    cache.insert(
        (method.to_ascii_uppercase(), url.to_string()),
        CacheEntry {
            status,
            headers: headers.to_vec(),
            body,
            etag,
            last_modified,
            stored_at: now_epoch(),
            final_url: final_url.to_string(),
            vary,
            req_headers: req_headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
                .collect(),
        },
    );
}

/// Drop the entry for `url`, if any. A successful unsafe request (POST/PUT/
/// DELETE/PATCH) invalidates the stored response for its target URL
/// (RFC 9111 §4.4), as do the URLs it redirects to or names in
/// `Location`/`Content-Location` (WPT `http-cache/invalidate`).
pub(crate) fn invalidate(url: &str) {
    CACHE
        .lock()
        .unwrap()
        .remove(&("GET".to_string(), url.to_string()));
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
                .find(|(ek, _)| ek.eq_ignore_ascii_case(k))
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
