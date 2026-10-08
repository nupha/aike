// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Request-header policy: Fetch-spec default headers and the forbidden
//! request-header filter (including the conditional method-override headers).

use crate::http::client::headers::HeaderList;

// Inject Fetch-spec default request headers (`Accept`, `Accept-Language`)
// when the caller did not supply them. Called after RequestInit/Request
// inheritance is applied, so explicit headers always win.
pub(super) fn ensure_default_request_headers(h: &mut HeaderList) {
    if !h.has("accept") {
        h.append("Accept", "*/*");
    }
    if !h.has("accept-language") {
        h.append("Accept-Language", "*");
    }
    // Browsers always send a User-Agent; WPT checks for its presence.
    if !h.has("user-agent") {
        h.append("User-Agent", "Mozilla/5.0 (aike)");
    }
}

/// WHATWG "forbidden request-header" names: a `fetch()` must silently drop any
/// header whose name (case-insensitively) is in this set, or begins with
/// `Proxy-` / `Sec-`. The browser (or underlying transport) owns these; a
/// script must not be able to set them. (Set-Cookie is also forbidden as a
/// request header but Headers does not expose it for sending anyway.)
///
/// The three `x-*` method-override headers are *conditionally* forbidden: only
/// when their value resolves to a forbidden method (see
/// `is_forbidden_override_value`).
fn is_forbidden_request_header(name: &str, value: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "accept-charset",
        "accept-encoding",
        "access-control-request-headers",
        "access-control-request-method",
        "connection",
        "content-length",
        "cookie",
        "cookie2",
        "date",
        "dnt",
        "expect",
        "host",
        "keep-alive",
        "origin",
        "referer",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "via",
    ];
    let lower = name.to_lowercase();
    if FORBIDDEN.contains(&lower.as_str()) {
        return true;
    }
    if lower.starts_with("proxy-") || lower.starts_with("sec-") {
        return true;
    }
    // `x-http-method` / `x-http-method-override` / `x-method-override`: forbidden
    // only when the value resolves to a forbidden method (CONNECT/TRACE/TRACK).
    if matches!(
        lower.as_str(),
        "x-http-method" | "x-http-method-override" | "x-method-override"
    ) {
        return is_forbidden_override_value(value);
    }
    false
}

/// WHATWG Fetch: the three method-override headers are forbidden when their
/// value, after "get, decode, and split" (split on commas, with quoted strings
/// kept intact and each item trimmed), yields a forbidden method
/// (CONNECT/TRACE/TRACK, byte-case-insensitive). A quoted method like
/// `"TRACE"` does NOT match (the quotes are part of the token), so a value such
/// as `",TRACE,"` is permitted.
fn is_forbidden_override_value(value: &str) -> bool {
    for method in split_on_commas(value) {
        if matches!(
            method.to_ascii_lowercase().as_str(),
            "connect" | "trace" | "track"
        ) {
            return true;
        }
    }
    false
}

/// Fetch "get, decode, and split" for a header value: split on commas, but a
/// comma inside an HTTP quoted string (`"..."`) is not a separator, and each
/// item is trimmed of surrounding whitespace. Quoted strings keep their quote
/// characters (so `"TRACE"` is a distinct token from `TRACE`). Backslash
/// escapes inside a quoted string are preserved verbatim.
fn split_on_commas(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let mut pos = 0;
    let mut out: Vec<String> = Vec::new();
    let mut temp = String::new();
    loop {
        while pos < chars.len() && chars[pos] != '"' && chars[pos] != ',' {
            temp.push(chars[pos]);
            pos += 1;
        }
        if pos < chars.len() && chars[pos] == '"' {
            // Collect the HTTP quoted string, keeping the quote characters.
            temp.push('"');
            pos += 1;
            while pos < chars.len() {
                let c = chars[pos];
                temp.push(c);
                pos += 1;
                if c == '"' {
                    break;
                }
                if c == '\\' && pos < chars.len() {
                    temp.push(chars[pos]);
                    pos += 1;
                }
            }
            if pos < chars.len() {
                continue;
            }
        }
        out.push(temp.trim().to_string());
        temp.clear();
        if pos >= chars.len() {
            return out;
        }
        if chars[pos] == ',' {
            pos += 1;
        }
    }
}

/// Drop all forbidden request headers set by the caller. Called after default
/// headers are injected (Accept / Accept-Language are not forbidden, so they
/// survive). The transport layer adds its own Content-Length / Host later.
///
/// `strip_cookies` controls the cookie headers: when fetch owns the cookie jar
/// (`cookie` switch on) `Cookie`/`Cookie2` are forbidden and dropped; otherwise
/// the caller may set them directly.
pub(super) fn strip_forbidden_request_headers(h: &mut HeaderList, strip_cookies: bool) {
    let drop: Vec<String> = h
        .to_vec()
        .into_iter()
        .filter(|(k, v)| {
            if !strip_cookies
                && (k.eq_ignore_ascii_case("cookie") || k.eq_ignore_ascii_case("cookie2"))
            {
                return false;
            }
            is_forbidden_request_header(k, v)
        })
        .map(|(k, _)| k)
        .collect();
    for k in drop {
        h.delete(&k);
    }
}
