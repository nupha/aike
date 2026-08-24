// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{error::Error, future::Future, pin::Pin, sync::Arc, time::Duration};

use base64::Engine;
use reqwest::cookie::CookieStore;
use zuqe::{ErrorKind, Exception, JSResult, JSValue, ScopedContext};

use super::headers::normalize_header_value;

/// Async gate invoked by the redirect loop before each hop that the request
/// owner flagged as needing one. This is a neutral extension point: the
/// WHATWG fetch layer installs a CORS-preflight gate here (browser builds);
/// everything else passes `None`. Arguments: `(client, hop URL, hop referrer,
/// credentials-stripped flag)`. An `Err(message)` aborts the whole fetch with
/// that error message.
pub(crate) type BeforeHopFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub(crate) type BeforeHopHook =
    Arc<dyn Fn(&reqwest::Client, &str, &str, bool) -> BeforeHopFuture + Send + Sync>;

/// Per-request data passed to [`perform_request`].
///
/// Contains only request-level fields.  Client-level settings (proxy, TLS,
/// cookie jar, redirect policy, …) belong in
/// [`HttpClientConfig`](super::config::HttpClientConfig) and are baked into
/// the `reqwest::Client` at construction time.
pub(crate) struct TransportRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    /// Referrer URL for the initial hop; "about:client" means no Referer header.
    pub referrer: String,
    /// Inputs needed to recompute the referrer on each redirect hop. `None`
    /// disables manual redirect following (the client's own policy applies).
    pub referrer_ctx: Option<ReferrerCtx>,
    /// Whether the body may be sent a second time. False for a stream body,
    /// which the first attempt consumes, disabling the 421 retry.
    pub replayable: bool,
    /// Per-hop gate (e.g. a CORS preflight), invoked before the initial hop
    /// and before every redirect hop the request is tainted on. `None` when
    /// no gate applies.
    pub before_hop: Option<BeforeHopHook>,
    /// Whether the request follows the WHATWG cors-mode rules (per-hop Origin
    /// recomputation and the opaque-origin rule on cross-origin redirects).
    pub cors_mode: bool,
    /// Whether the initial request is CORS-tainted (cors mode + cross-origin).
    pub cors_tainted: bool,
    /// Serialized client origin (the document origin). Used as the `Origin`
    /// value; an opaque origin (after a cross-origin redirect of a cors request)
    /// is the literal `null`.
    pub cors_client_origin: String,
    /// The fetch `credentials` mode: "omit" | "same-origin" | "include". Drives
    /// whether cookies are sent/stored (via the shared global jar).
    pub credentials_mode: String,
    /// Whether this request manages cookies manually via the shared global jar
    /// (the WHATWG `fetch` path). The low-level HTTP client API keeps reqwest's
    /// own per-client cookie store and does not touch the global jar.
    pub use_shared_cookies: bool,
}

/// Policy inputs carried across redirects so each hop can recompute its own
/// `Referer` value. Without this the initial header would be replayed verbatim,
/// which breaks `origin-when-cross-origin` when a hop crosses origins.
pub(crate) struct ReferrerCtx {
    /// The request's `referrer` field ("about:client" or an explicit URL).
    pub referrer: String,
    pub policy: String,
    pub client_origin: Option<String>,
    pub client_url: Option<String>,
    /// Maximum hops to follow before reporting a redirect error.
    pub max_redirects: usize,
}

/// Result of [`perform_request`]. `resp` is kept open for lazy body streaming.
pub(crate) struct TransportResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub content_type: String,
    pub content_length: Option<u64>,
    /// Negotiated HTTP version: "0.9" | "1.0" | "1.1" | "2" | "3".
    pub http_version: &'static str,
    pub resp: Option<reqwest::Response>,
    /// Final URL after redirects. Recorded before `resp` is taken/consumed so
    /// the fetch layer always has the post-redirect URL even after the body is
    /// read (the `Response.url` property must reflect the last URL in the chain).
    pub final_url: String,
    /// Pre-decoded body for non-network responses (e.g. `data:` URLs).
    pub synthetic_body: Option<Vec<u8>>,
    /// The request's origin became opaque ("null") via a cross-origin redirect.
    /// The WHATWG fetch layer runs the final CORS check against the request's
    /// actual origin, which is "null" in that case (not the client origin).
    pub origin_opaque: bool,
}

/// Decode a `data:` URL (RFC 2397) into body bytes and a media type.
/// Returns `(content_type, body)`. Decoding failures produce an error
/// description string rather than panicking.
fn decode_data_url(url: &str) -> Result<(String, Vec<u8>), String> {
    // Strip the "data:" prefix (already validated by caller).
    let rest = &url["data:".len()..];
    // Split mediatype from data at the first comma.
    let (meta, data) = match rest.find(',') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => return Err("data: URL has no comma separator".into()),
    };
    let is_base64 = meta.to_ascii_lowercase().ends_with(";base64");
    let media_type = if is_base64 {
        &meta[..meta.len() - ";base64".len()]
    } else {
        meta
    };
    let content_type = if media_type.is_empty() {
        "text/plain;charset=US-ASCII".to_string()
    } else {
        media_type.to_string()
    };

    // Payload is percent-decoded unless base64.
    let bytes = if is_base64 {
        let trimmed: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(trimmed)
            .map_err(|e| format!("invalid base64 in data: URL: {e}"))?
    } else {
        percent_decode(data.as_bytes())
    };
    Ok((content_type, bytes))
}

/// Fetch spec: multiple identical `Content-Length` response headers combine into
/// a single header; conflicting values are all dropped (treated as absent).
fn combine_content_length_headers(raw: Vec<(String, String)>) -> Vec<(String, String)> {
    let values: Vec<&str> = raw
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.as_str())
        .collect();
    if values.len() <= 1 {
        return raw;
    }
    if values.iter().all(|v| *v == values[0]) {
        let mut out = Vec::with_capacity(raw.len());
        let mut merged = false;
        for (k, v) in raw {
            if k.eq_ignore_ascii_case("content-length") {
                if merged {
                    continue;
                }
                out.push((k, v));
                merged = true;
            } else {
                out.push((k, v));
            }
        }
        out
    } else {
        raw.into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
            .collect()
    }
}

/// Canonical HTTP reason phrase for a status code (IANA registry). The Fetch
/// spec exposes the response's status message via `Response.statusText`; for a
/// real HTTP response this is the status-line reason phrase (200 → "OK",
/// 304 → "Not Modified", ...). Unknown codes fall back to the empty string.
fn canonical_status_text(status: u16) -> String {
    let phrase = match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    };
    phrase.to_string()
}

/// Percent-decode a `data:`-URL payload (so the body carries the raw octets,
/// e.g. `%EF%BB%BF` -> UTF-8 BOM bytes). Not a full URI decode: only the
/// escapes matter here, and `+` is not special in this context.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'%' if i + 2 < input.len() => {
                let hi = (input[i + 1] as char).to_digit(16);
                let lo = (input[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(input[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Transport failure with a machine-readable category.
///
/// `code` is one of: "timeout" | "connect" | "dns" | "tls" | "redirect" | "other".
pub(crate) struct TransportError {
    pub code: &'static str,
    pub message: String,
}

/// Classify a reqwest error into a short error code for JS-side dispatch.
fn classify(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        return "timeout";
    }
    if e.is_redirect() {
        return "redirect";
    }
    // DNS / TLS causes are buried in the source chain; sniff before the
    // generic connect check (a TLS handshake failure is also is_connect).
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        let msg = s.to_string().to_lowercase();
        if msg.contains("dns") {
            return "dns";
        }
        if msg.contains("certificate") || msg.contains("tls") || msg.contains("handshake") {
            return "tls";
        }
        src = s.source();
    }
    if e.is_connect() {
        return "connect";
    }
    "other"
}

/// Map `reqwest::Version` to a short version string.
fn version_str(v: reqwest::Version) -> &'static str {
    match v {
        reqwest::Version::HTTP_09 => "0.9",
        reqwest::Version::HTTP_10 => "1.0",
        reqwest::Version::HTTP_11 => "1.1",
        reqwest::Version::HTTP_2 => "2",
        reqwest::Version::HTTP_3 => "3",
        _ => "",
    }
}

/// Throw a JS `Error` carrying the transport error `code` property, so JS
/// can dispatch on it (e.g. retry on "timeout" but not on "tls").
pub(crate) fn throw_transport_error(cx: &mut ScopedContext, te: TransportError) -> JSResult {
    let mut err = match cx.create_error_object(ErrorKind::Type, &te.message, false) {
        Some(e) => e,
        None => return Err(-1),
    };
    let code = cx.create_string_from_utf8(te.code.as_bytes().to_vec())?;
    err.define_property_value_str(
        cx,
        "code",
        JSValue::make_string(code),
        zuqe::JS_PROP_WRITABLE | zuqe::JS_PROP_CONFIGURABLE,
    )?;
    cx.throw(Exception::Error(err.into()));
    Err(-1)
}

/// Follow redirects by hand, recomputing the `Referer` for every hop and, for
/// `cors`-mode requests, the `Origin` header (Fetch spec redirect handling).
///
/// The client's own redirect policy replays the initial request headers on
/// each hop, which is wrong for `origin-when-cross-origin` (once a hop crosses
/// origins the referrer must collapse to the origin) and for cors requests
/// (a cross-origin redirect makes the origin opaque → `Origin: null`). So the
/// caller disables auto-follow and we re-issue each hop with freshly computed
/// referrer and origin.
///
/// `send` receives `(url, referrer, cors_tainted, origin_opaque)` per hop.
/// `cors_tainted` tracks whether the request is currently CORS-tainted;
/// `origin_opaque` becomes true after a cross-origin redirect of a tainted
/// request, after which the `Origin` header is the literal `null`.
///
/// `preflight`, when `Some`, runs a CORS OPTIONS preflight before each tainted
/// hop's actual request (Fetch spec: a redirect re-enters HTTP-network-or-cache
/// fetch, which re-runs CORS-preflight for the new URL). A failed preflight
/// aborts the whole fetch with a "cors" network error.
async fn follow_redirects<F, Fut>(
    rc: &ReferrerCtx,
    start_url: &str,
    start_referrer: &str,
    start_method: &str,
    cors_mode: bool,
    cors_tainted: bool,
    cors_client_origin: &str,
    client: &reqwest::Client,
    before_hop: Option<&BeforeHopHook>,
    send: F,
) -> Result<(reqwest::Response, bool), TransportError>
where
    F: Fn(String, String, String, bool, bool) -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let mut url = start_url.to_string();
    let mut referrer = start_referrer.to_string();
    // The referrer already determined for the *initial* request (computed with
    // the init policy against the same-origin initial URL). Redirect responses
    // can only tighten it, never loosen it.
    let initial_referrer = referrer.clone();
    // The request method, which Fetch rewrites to GET on a 301/302/303 redirect
    // of a non-GET/HEAD request (history-preserving behavior; 303 always, and
    // 301/302 per the ubiquitous POST→GET convention the WPT suite asserts).
    let mut method = start_method.to_string();
    // The effective referrer policy, which a redirect response may tighten via
    // its `Referrer-Policy` response header (Fetch spec: the redirect's policy
    // overrides the initiating request's policy for the *next* hop).
    let mut policy = rc.policy.clone();
    let mut tainted = cors_tainted;
    let mut origin_opaque = false;
    // Origin of the previous hop (initially the start URL). When a redirect
    // changes origin, credentials headers (Authorization, Cookie,
    // Proxy-Authorization) must be stripped so they don't leak cross-origin
    // (Fetch spec HTTP-redirect fetch).
    let mut prev_url = start_url.to_string();
    // Set once a hop is reached via a cross-origin redirect: the credentials
    // headers (Authorization, Cookie, Proxy-Authorization) were stripped, so a
    // preflight whose only unsafe headers were those credentials is no longer
    // needed for any subsequent hop.
    let mut credentials_stripped = false;
    // Whether we have already followed at least one redirect. A request whose
    // initial method is non-safe (e.g. POST) must not emit an `Origin` header on
    // any *subsequent* hop, since POST redirects are treated as potentially
    // leaking and browsers strip `Origin`/`Authorization` across them (WPT
    // `redirect-origin.any.js`).
    for _ in 0..=rc.max_redirects {
        // A cross-origin redirect (current URL's origin differs from the
        // previous hop's) strips the credentials headers on this hop.
        let cross_origin_hop = origin_of(&url) != origin_of(&prev_url);
        if cross_origin_hop {
            credentials_stripped = true;
        }

        // A CORS-tainted hop runs the owner's per-hop gate before the actual
        // request (browser builds: a CORS OPTIONS preflight against this hop's
        // URL — Fetch spec: a redirect re-enters HTTP-network-or-cache fetch,
        // which re-runs CORS-preflight for the new URL). A failed gate is a
        // network error and the fetch never reaches the target.
        if tainted
            && let Some(hook) = before_hop
            && let Err(msg) = hook(client, &url, &referrer, credentials_stripped).await
        {
            return Err(TransportError {
                code: "cors",
                message: msg,
            });
        }

        let resp = send(
            method.clone(),
            url.clone(),
            referrer.clone(),
            origin_opaque,
            cross_origin_hop,
        )
        .await
        .map_err(|e| TransportError {
            code: classify(&e),
            message: format!("request failed: {e}"),
        })?;

        // A redirect without a usable `Location` is delivered as-is; WPT serves
        // `.any.js` via a 302 with an empty `Location` (self-redirect) so the
        // engine must tolerate it and surface the response rather than reject.
        let next = match redirect_target(&resp, &url) {
            Some(u) => u,
            None => return Ok((resp, origin_opaque)),
        };

        // A redirect response's `Referrer-Policy` header overrides the policy
        // used to compute the referrer for the *next* hop (Fetch spec). This is
        // how a server can tighten the referrer after a redirect (e.g. WPT
        // `redirect-referrer-override`).
        if let Some(hdr) = resp
            .headers()
            .get(reqwest::header::HeaderName::from_static("referrer-policy"))
        {
            if let Ok(v) = hdr.to_str() {
                let v = v.trim().to_ascii_lowercase();
                if !v.is_empty() {
                    policy = v;
                }
            }
        }

        // 301/302/303 rewrite a non-GET/HEAD method to GET for the next hop (the
        // ubiquitous POST→GET behavior; 303 always does). The request body is
        // dropped for GET automatically in `send_once`.
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303) && !method_is_safe(&method) {
            method = "GET".to_string();
        }

        // Fetch spec: a cors request must not follow a redirect to a URL that
        // carries credentials (userinfo); that is a network error.
        if cors_mode
            && url::Url::parse(&next)
                .map(|u| u.username() != "" || u.password().is_some())
                .unwrap_or(false)
        {
            return Err(TransportError {
                code: "cors",
                message: format!("request failed: redirect to a URL with credentials ({next})"),
            });
        }

        // Opaque-origin rule: a cors request whose redirect target's origin
        // differs from the current URL's origin gets a unique opaque origin,
        // so its `Origin` header becomes `null`.
        if cors_mode && tainted && origin_of(&url) != origin_of(&next) {
            origin_opaque = true;
        }
        // A cors-mode request becomes CORS-tainted once the current URL is
        // cross-origin with the client (relevant for a same-origin → cross
        // redirect, which starts untamed). `null` (opaque client origin) is
        // always treated as cross-origin.
        let client_is_opaque = cors_client_origin == "null";
        if cors_mode
            && (client_is_opaque || origin_of(&next) != Some(cors_client_origin.to_string()))
        {
            tainted = true;
        }

        // Recompute the referrer against the *next* URL so a hop that changes
        // origin gets the origin-only form.
        //
        // Per the Fetch spec + WPT `redirect-referrer-override`, the referrer of
        // a hop that follows a redirect is the *more conservative* of:
        //   (a) the referrer already determined for the initial request
        //       (`initial_referrer`, computed with the init policy against the
        //       same-origin initial URL), and
        //   (b) the referrer recomputed with the redirect response's
        //       `Referrer-Policy` against the new (possibly cross-origin) URL.
        // A redirect response only ever *tightens* the referrer. An omitted
        // referrer (`""`) always wins, and an origin-only form
        // (`http://h:port/`) is more conservative than a full URL.
        let ref_resp = crate::whatwg::fetch::compute_request_referrer(
            &rc.referrer,
            &policy,
            rc.client_origin.as_deref(),
            rc.client_url.as_deref(),
            &next,
        )
        .unwrap_or_default();
        referrer = if initial_referrer.is_empty() || ref_resp.is_empty() {
            String::new()
        } else if initial_referrer.len() <= ref_resp.len() {
            initial_referrer.clone()
        } else {
            ref_resp
        };
        prev_url = url;
        url = next;
    }

    Err(TransportError {
        code: "redirect",
        message: format!("too many redirects (limit {})", rc.max_redirects),
    })
}

/// Origin (scheme://host[:port]) of a URL string, or `None` if unparsable.
fn origin_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
}

/// Whether a method is a "safe"/idempotent method (GET or HEAD, case-insensitive).
/// The Fetch spec appends an `Origin` header for any other method.
fn method_is_safe(method: &str) -> bool {
    method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
}

/// Whether cookies may be sent/stored for a request per its `credentials` mode:
/// "include" always; "same-origin" only when the target URL's origin matches
/// the client origin; "omit" never.
fn cookies_allowed(credentials_mode: &str, client_origin: &str, url: &str) -> bool {
    match credentials_mode {
        "include" => true,
        "same-origin" => url::Url::parse(url)
            .ok()
            .map(|u| u.origin().ascii_serialization() == client_origin)
            .unwrap_or(false),
        _ => false,
    }
}

/// Resolve a redirect response's `Location` against the current URL.
/// Returns `None` when the response is not a redirect or has no valid target.
fn redirect_target(resp: &reqwest::Response, current: &str) -> Option<String> {
    if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }

    let loc = resp
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()?;
    let base = url::Url::parse(current).ok()?;
    let joined = base.join(loc).ok()?;
    let joined = joined.to_string();
    Some(joined)
}

/// Perform an HTTP request on a pre-built `reqwest::Client`.
///
/// The caller is responsible for building the client with the desired
/// proxy / TLS / cookie / redirect settings via [`HttpClientConfig`].
pub(crate) async fn perform_request(
    client: &reqwest::Client,
    req: TransportRequest,
) -> Result<TransportResponse, TransportError> {
    // `data:` URLs are not network requests — decode them directly.
    if req.url.to_ascii_lowercase().starts_with("data:") {
        let (content_type, body) = decode_data_url(&req.url).map_err(|e| TransportError {
            code: "other",
            message: e,
        })?;
        // HEAD requests carry no body (fetch spec), even for `data:` URLs.
        let is_head = req.method.eq_ignore_ascii_case("HEAD");
        return Ok(TransportResponse {
            status: 200,
            // Non-network (synthetic) responses such as `data:` URLs get the
            // default status 200 with statusText "OK" and type "default", per
            // the WHATWG fetch scheme fetch algorithm. (Network HTTP responses
            // keep statusText "" — the reason phrase is not exposed to scripts.)
            status_text: "OK".to_string(),
            headers: vec![("content-type".into(), content_type.clone())],
            content_type,
            content_length: Some(body.len() as u64),
            http_version: "1.1",
            resp: None,
            final_url: req.url.clone(),
            synthetic_body: if is_head { None } else { Some(body) },
            origin_opaque: false,
        });
    }

    // WHATWG fetch: a 421 (Misdirected Request) response means the connection
    // was reused for the wrong origin; retry the request exactly once on a
    // fresh connection. The reqwest client is built with
    // `pool_max_idle_per_host(0)`, so a second attempt opens a clean
    // connection (the server counts it as a new connection).
    // Build one request. Owned args keep the returned future free of borrows,
    // so it can be handed to the redirect loop. `cors_tainted`/`origin_opaque`
    // drive the per-hop `Origin` header (Fetch spec redirect handling).
    let send_once = |method: String,
                     url: String,
                     referrer: String,
                     origin_opaque: bool,
                     cross_origin_hop: bool| {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut rb = client.request(method.clone(), &url).timeout(req.timeout);

        let mut headers = req.headers.clone();
        // A cross-origin redirect must not leak the request's credentials
        // (Fetch spec HTTP-redirect fetch): strip `Authorization`,
        // `Proxy-Authorization`, and `Cookie` so they don't reach the new
        // origin. Cookies are re-added from the jar below only if the
        // `credentials` mode permits them for this hop's origin.
        if cross_origin_hop {
            headers.retain(|(k, _)| {
                !k.eq_ignore_ascii_case("authorization")
                    && !k.eq_ignore_ascii_case("proxy-authorization")
                    && !k.eq_ignore_ascii_case("cookie")
            });
        }
        if referrer != "about:client" && !referrer.is_empty() {
            let has_referer = headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("referer"));
            if !has_referer {
                headers.push(("Referer".to_string(), referrer));
            }
        }
        // For `cors`-mode requests, recompute the `Origin` header for this hop.
        // Per the WHATWG fetch "append a request `Origin` header" algorithm the
        // value is set whenever the method is not GET/HEAD, or when the request is
        // CORS-tainted (a cross-origin `cors`-mode hop) — regardless of whether the
        // *current* URL is cross-origin. So a same-origin `cors`-mode POST still
        // emits `Origin` (= client origin), and a cross-origin GET does too. An
        // opaque origin (turned opaque by a cross-origin redirect) serializes to the
        // literal `null`. `Origin` is a forbidden request header, so any
        // caller-provided value is replaced. Non-cors requests keep whatever
        // `Origin` the caller set.
        if req.cors_mode {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("origin"));
            let origin_value: Option<String> = if origin_opaque {
                Some("null".to_string())
            } else if !method_is_safe(method.as_str()) || cross_origin_hop {
                Some(req.cors_client_origin.clone())
            } else {
                None
            };
            if let Some(ov) = origin_value {
                headers.push(("Origin".to_string(), ov));
            }
        }

        // Fetch cookies from the shared jar when the `credentials` mode permits
        // (include, or same-origin for a same-origin URL). `omit` never sends.
        if req.use_shared_cookies
            && cookies_allowed(&req.credentials_mode, &req.cors_client_origin, &url)
            && let Ok(u) = url::Url::parse(&url)
        {
            if let Some(cv) = crate::http::client::config::global_cookie_jar().cookies(&u) {
                headers.retain(|(k, _)| !k.eq_ignore_ascii_case("cookie"));
                if let Ok(s) = cv.to_str() {
                    // Present cookies in creation order (WPT `cookies.any.js`).
                    let s = crate::http::client::config::reorder_cookie_header(s);
                    headers.push(("Cookie".to_string(), s));
                }
            }
        }

        for (k, v) in &headers {
            // Fetch spec: strip leading/trailing HTTP whitespace (0x09/0x0A/
            // 0x0D/0x20) from header values before they hit the wire.
            let v = normalize_header_value(v);

            // Header values are latin-1 strings (validated in JS). Encode to raw
            // bytes and bypass the http crate's validation so control characters
            // permitted by the Fetch spec (e.g. 0x01) are sent verbatim, matching
            // WPT header-values-normalize expectations. Requires the `http` crate
            // to be compiled without debug-assertions (see aike/Cargo.toml) so
            // `from_maybe_shared_unchecked` skips its UTF-8/byte validity check.
            let bytes: Vec<u8> = v
                .chars()
                .filter_map(|c| {
                    let cp = c as u32;
                    if cp <= 0xFF { Some(cp as u8) } else { None }
                })
                .collect();
            let hv = unsafe { reqwest::header::HeaderValue::from_maybe_shared_unchecked(bytes) };
            rb = rb.header(k, hv);
        }

        // A redirect rewrites the method to GET (see `follow_redirects`); a GET
        // request must not carry the original request body, so only attach it for
        // non-GET/HEAD methods.
        if !method_is_safe(&method.to_string()) {
            if let Some(b) = &req.body {
                rb = rb.body(b.clone());
            }
        }
        async move { rb.send().await }
    };

    // One attempt at `url`, transparently retried once on a 421 — but only for
    // a replayable body. A stream body is consumed by the first attempt, so the
    // spec requires the 421 to be surfaced instead.
    let replayable = req.replayable;
    let send_with_retry = move |method: String,
                                url: String,
                                referrer: String,
                                origin_opaque: bool,
                                cross_origin_hop: bool| {
        let first = send_once(
            method.clone(),
            url.clone(),
            referrer.clone(),
            origin_opaque,
            cross_origin_hop,
        );
        async move {
            match first.await {
                Ok(r) if replayable && r.status() == reqwest::StatusCode::MISDIRECTED_REQUEST => {
                    log::debug!("421 on {url}; retrying once on a fresh connection");
                    send_once(method, url, referrer, origin_opaque, cross_origin_hop).await
                }
                other => other,
            }
        }
    };

    let cors_tainted = req.cors_tainted;
    let cors_client_origin = req.cors_client_origin.clone();
    let cors_mode = req.cors_mode;
    let before_hop = req.before_hop.as_ref();
    // Whether the request's origin became opaque (a unique opaque origin) via a
    // cross-origin redirect. Reported on the response so the WHATWG fetch layer
    // can run the final CORS check against the request's actual origin ("null").
    let mut resp_origin_opaque = false;
    let resp = match &req.referrer_ctx {
        Some(rc) => {
            let (r, opaque) = follow_redirects(
                rc,
                &req.url,
                &req.referrer,
                &req.method,
                cors_mode,
                cors_tainted,
                &cors_client_origin,
                client,
                before_hop,
                send_with_retry,
            )
            .await?;
            resp_origin_opaque = opaque;
            r
        }
        None => {
            if cors_tainted
                && let Some(hook) = before_hop
                && let Err(msg) = hook(client, &req.url, &req.referrer, false).await
            {
                return Err(TransportError {
                    code: "cors",
                    message: msg,
                });
            }
            send_with_retry(
                req.method.clone(),
                req.url.clone(),
                req.referrer.clone(),
                false,
                false,
            )
            .await
            .map_err(|e| {
                let mut msg = format!("request failed: {e}");
                let mut src = e.source();
                while let Some(s) = src {
                    msg.push_str(&format!("; caused by: {s}"));
                    src = s.source();
                }
                TransportError {
                    code: classify(&e),
                    message: msg,
                }
            })?
        }
    };

    // Store `Set-Cookie` response cookies into the shared jar when the
    // `credentials` mode permits, so a later `fetch()` (with include/same-origin)
    // to the same origin sends them (WPT cors-cookies).
    if req.use_shared_cookies
        && cookies_allowed(
            &req.credentials_mode,
            &cors_client_origin,
            resp.url().as_str(),
        )
    {
        let url_for_cookie = resp.url().clone();
        let jar = crate::http::client::config::global_cookie_jar();
        for (k, v) in resp.headers() {
            if k == reqwest::header::SET_COOKIE
                && let Ok(s) = v.to_str()
            {
                jar.add_cookie_str(s, &url_for_cookie);
                // Track the first-seen order for the `Cookie` header (WPT).
                crate::http::client::config::cookie_order_record(s);
            }
        }
    }

    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Decode values as latin-1, not UTF-8: a header value is a byte string and
    // bytes 0x80-0xFF are legal (RFC 7230 obs-text). `to_str()` rejects those,
    // so using it here would silently drop the whole header.
    let raw_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            let vs = v.as_bytes().iter().map(|&b| b as char).collect::<String>();
            (k.to_string(), vs)
        })
        .collect();
    // Fetch spec: multiple identical `Content-Length` headers combine into one;
    // conflicting `Content-Length` values are dropped (treated as absent).
    let headers = combine_content_length_headers(raw_headers);
    // `Response.statusText` is the response's status message: the HTTP reason
    // phrase for a real response (200 → "OK", 304 → "Not Modified", ...).
    // HTTP/2 has no reason phrase, so the spec mandates an empty `statusText`
    // for h2 responses regardless of the status code (WPT `status.h2.any.js`).
    let http_ver = version_str(resp.version());
    let status_text = if http_ver.starts_with("HTTP/2") {
        String::new()
    } else {
        canonical_status_text(status.as_u16())
    };
    // Read Content-Length from the header: resp.content_length() reports the
    // body size hint, which is erased by the auto-decompression wrapper.
    let content_length = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let final_url_val = resp.url().to_string();
    let mut tr = TransportResponse {
        status: status.as_u16(),
        status_text,
        headers,
        content_type,
        content_length,
        http_version: version_str(resp.version()),
        resp: Some(resp),
        final_url: final_url_val,
        synthetic_body: None,
        origin_opaque: resp_origin_opaque,
    };
    // WHATWG fetch: 204/205/304 responses and responses to HEAD requests carry
    // no body, so the JS-visible `Response.body` is null and `text()`/etc.
    // resolve to the empty string. Drop the underlying transport so body
    // consumers treat it as a null-body response.
    if req.method.eq_ignore_ascii_case("HEAD") || matches!(tr.status, 204 | 205 | 304) {
        tr.resp = None;
        tr.synthetic_body = None;
    }
    Ok(tr)
}
