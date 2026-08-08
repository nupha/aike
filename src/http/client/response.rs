// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! HTTP response classes.
//!
//! `Response` is the strict WHATWG class returned by `fetch()`. It exposes
//! the `Response` constructor, `error()` / `redirect()` / `json()` statics,
//! and the `Body` mixin.
//!
//! `HttpClientResponse` (Rust type `JSHttpClientResponse`) extends `Response` and is
//! returned by `HttpClient`. It adds aike enhancements: `contentType`,
//! `contentLength`, `httpVersion`, `close()`. Both classes share
//! `ResponsePayload` and the same native callbacks; only the ClassID and
//! prototype differ.

use std::cell::RefCell;
use std::rc::Rc;

use crossbeam::channel::Receiver;
use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, Exception, FieldDef, JSResult, JSValue, NativeObject,
    Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    html::HtmlConvertMode,
    http::client::{
        body::{
            BODY_AS_ARRAY_BUFFER, BODY_AS_BLOB, BODY_AS_BYTES, BODY_AS_FORM_DATA, BODY_AS_JSON,
            BODY_AS_TEXT, BODY_AS_TEXT_CONVERT, BodySource, blob_type_from_header,
            body_content_type, body_to_bytes, drain_body,
        },
        headers::{Guard, HeaderList, HeadersPayload, JSHeaders, header_list_from_js},
        transport::TransportResponse,
    },
    whatwg::{abort::signal_reason, stream::JSReadableStream},
};

/// WHATWG `Response.type` (closed set of values this runtime produces).
/// `Error` / `OpaqueRedirect` / `Opaque` force `body` to null per spec.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RespType {
    Basic,
    Cors,
    Default,
    Error,
    OpaqueRedirect,
    Opaque,
}

impl RespType {
    /// Spec string exposed via the `type` getter.
    fn as_str(self) -> &'static str {
        match self {
            RespType::Basic => "basic",
            RespType::Cors => "cors",
            RespType::Default => "default",
            RespType::Error => "error",
            RespType::OpaqueRedirect => "opaqueredirect",
            RespType::Opaque => "opaque",
        }
    }
}

pub(crate) struct ResponsePayload {
    /// Open network response for lazy body streaming (None once taken).
    resp: Option<reqwest::Response>,
    /// Transient holder for the body channel while it is being consumed.
    /// Each item is `Ok(bytes)` for a chunk or `Err(msg)` on a network failure.
    body_rx: Option<Receiver<Result<Vec<u8>, String>>>,
    /// Whether the body has begun consumption (enforces single-use).
    body_consumed: bool,

    /// Cached `ReadableStream` returned by `response.body` (same instance each
    /// access; locked once the body is consumed).
    pub(crate) cached_body: Option<JSReadableStream>,
    /// Fixed body bytes for constructed (synthetic) responses.
    pub(crate) synthetic_body: Option<Vec<u8>>,

    /// `ReadableStream` passed to `new Response(stream)`; drained on consumption.
    ctor_stream: Option<JSReadableStream>,

    /// WHATWG `Response.type`; see [`RespType`].
    resp_type: RespType,

    pub(crate) url: String,
    redirected: bool,
    pub(crate) status: u16,
    pub(crate) status_text: String,
    content_type: String,
    /// Shared live header list. The exposed `Headers` object references the
    /// same `Rc`, so user mutations via `response.headers.set()` are visible to
    /// body consumers (e.g. `blob()` type derivation).
    pub(crate) headers: Rc<RefCell<HeaderList>>,
    /// Cached `Headers` object returned by the `headers` getter ([SameObject]).
    headers_obj: Option<JSValue>,
    /// Guard applied to the exposed `Headers`. Responses built by `fetch()` (or
    /// `Response.error()`) are immutable; ones built by user code use the
    /// "response" guard, which is mutable but drops forbidden response headers.
    headers_guard: Guard,
    content_length: Option<u64>,
    /// Negotiated HTTP version ("1.1", "2", …); empty for synthetic responses.
    http_version: &'static str,
    /// Abort signal tied to this response's fetch. When aborted mid-body, the
    /// body `ReadableStream` errors with the signal's reason (WHATWG Fetch: an
    /// abort rejects the in-flight `Response.body` reader). Kept alive for GC.
    signal: Option<JSValue>,
}

impl ResponsePayload {
    /// Keep JS-visible stream handles alive across GC while the body is held.
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.cached_body {
            s.trace(gcx);
        }
        if let Some(s) = self.ctor_stream {
            JSValue::from(s).trace(gcx);
        }
        if let Some(ref h) = self.headers_obj {
            h.trace(gcx);
        }
        if let Some(ref s) = self.signal {
            s.trace(gcx);
        }
    }

    /// The relevant body stream: the constructor-supplied stream (if present)
    /// over the lazily-cached `.body` stream. Borrows `self`.
    fn underlying_stream(&self) -> Option<&JSReadableStream> {
        self.ctor_stream.as_ref().or(self.cached_body.as_ref())
    }

    /// Per fetch spec: `bodyUsed` is true once the body has been consumed OR
    /// its underlying stream is disturbed (read/cancelled). A merely-locked
    /// stream (getReader only) does NOT disturb.
    fn body_disturbed(&self) -> bool {
        self.underlying_stream()
            .and_then(|s| s.payload())
            .is_some_and(|sp| sp.disturbed)
    }

    /// Whether the body cannot be re-consumed: already consumed, or its
    /// underlying stream is disturbed/locked by a previous reader.
    fn body_unusable(&self) -> bool {
        self.body_consumed
            || self
                .underlying_stream()
                .and_then(|s| s.payload())
                .is_some_and(|sp| sp.disturbed || sp.reader.is_some())
    }

    /// Start body streaming (lazily) and return a `BodySource`.
    /// Returns `Err` if the body was already consumed or is unavailable.
    fn ensure_cached_body(&mut self) -> Result<BodySource, i32> {
        if self.body_consumed {
            return Err(-1);
        }

        if let Some(resp) = self.resp.take() {
            let (tx, rx) = crossbeam::channel::unbounded::<Result<Vec<u8>, String>>();
            self.body_rx = Some(rx);

            tokio::spawn(async move {
                use futures_util::StreamExt;

                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if tx.send(Ok(bytes.to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            // Signal the consumer that the body stream failed:
                            // the reader's read() promise must reject (TypeError).
                            let _ = tx.send(Err(format!("{e}")));
                            break;
                        }
                    }
                }
            });
            Ok(BodySource::Channel(self.body_rx.clone().unwrap()))
        } else if let Some(bytes) = self.synthetic_body.clone() {
            Ok(BodySource::Synthetic(bytes))
        } else if let Some(stream) = self.ctor_stream {
            // Body given to `new Response(stream)` — drain the stream itself.
            Ok(BodySource::Stream(stream))
        } else {
            Err(-1)
        }
    }
}

fn get_response_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut ResponsePayload, i32> {
    // HttpClientResponse is a subclass with its own ClassID; probe both classes.
    let p = if JSResponse::payload_of(val).is_some() {
        JSResponse::payload_mut_of(val)
    } else {
        JSHttpClientResponse::payload_mut_of(val)
    };
    p.ok_or_else(|| ctx.throw_type_error("not an http response object"))
}

fn get_response_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a ResponsePayload, i32> {
    JSResponse::payload_of(val)
        .or_else(|| JSHttpClientResponse::payload_of(val))
        .ok_or_else(|| ctx.throw_type_error("not an http response object"))
}

/// Build an opaque-redirect `Response` (used for `redirect: "manual"`).
pub(crate) fn build_opaque_redirect_response(ctx: &mut ScopedContext, url: &str) -> JSResult {
    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: true,
            cached_body: None,
            synthetic_body: None,
            ctor_stream: None,
            resp_type: RespType::OpaqueRedirect,
            url: url.to_string(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers: Rc::new(RefCell::new(HeaderList::new())),
            content_length: None,
            http_version: "",
            signal: None,
            headers_obj: None,
            headers_guard: Guard::Immutable,
        },
    )
    .map(JSValue::from)
}

/// CORS-safelisted response headers exposed on an opaque filtered response
/// (Fetch spec §5.1.1). Non-safelisted headers (e.g. x-is-filtered) are dropped.
const CORS_SAFELISTED_HEADERS: &[&str] = &[
    "cache-control",
    "content-language",
    "content-length",
    "content-type",
    "expires",
    "last-modified",
    "pragma",
];

/// Build an opaque `Response` (used for a cross-origin `no-cors` fetch).
///
/// Per Fetch spec, an opaque response has `type="opaque"`, `status=0`,
/// `statusText=""`, `url=""`, a null body, and a header list filtered down to
/// the CORS-safelisted set. The real network body is dropped (`resp: None`).
pub(crate) fn build_opaque_response(ctx: &mut ScopedContext, tr: TransportResponse) -> JSResult {
    let headers: Vec<(String, String)> = tr
        .headers
        .into_iter()
        .filter(|(k, _)| {
            CORS_SAFELISTED_HEADERS
                .iter()
                .any(|h| h.eq_ignore_ascii_case(k))
        })
        .collect();
    let headers = Rc::new(RefCell::new(HeaderList::from_vec(headers)));

    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: true,
            cached_body: None,
            synthetic_body: None,
            ctor_stream: None,
            resp_type: RespType::Opaque,
            url: String::new(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers,
            content_length: None,
            http_version: "",
            signal: None,
            headers_obj: None,
            headers_guard: Guard::Immutable,
        },
    )
    .map(JSValue::from)
}

/// Build the shared payload from a [`TransportResponse`].
fn transport_payload(
    tr: TransportResponse,
    resp_type: RespType,
    url: &str,
    redirected: bool,
    signal: Option<JSValue>,
) -> ResponsePayload {
    ResponsePayload {
        resp: tr.resp,
        body_rx: None,
        body_consumed: false,
        cached_body: None,
        synthetic_body: tr.synthetic_body,
        ctor_stream: None,
        resp_type,
        url: url.to_string(),
        redirected,
        status: tr.status,
        status_text: tr.status_text,
        content_type: tr.content_type,
        headers: Rc::new(RefCell::new(HeaderList::from_vec(tr.headers))),
        headers_obj: None,
        // Came off the network: headers are immutable.
        headers_guard: Guard::Immutable,
        content_length: tr.content_length,
        http_version: tr.http_version,
        signal,
    }
}

/// Build a plain WHATWG `Response` from a [`TransportResponse`] (fetch path).
pub(crate) fn build_response_from_transport(
    ctx: &mut ScopedContext,
    tr: TransportResponse,
    resp_type: RespType,
    url: &str,
    redirected: bool,
    signal: Option<JSValue>,
) -> JSResult {
    // An opaque response drops the real status/url/body and filters headers to
    // the CORS-safelisted set (Fetch spec §5.1.1).
    if resp_type == RespType::Opaque {
        return build_opaque_response(ctx, tr);
    }
    let payload = transport_payload(tr, resp_type, url, redirected, signal);
    JSResponse::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

/// Build an enhanced `HttpClientResponse` from a [`TransportResponse`] (HttpClient path).
pub(crate) fn build_http_response_from_transport(
    ctx: &mut ScopedContext,
    tr: TransportResponse,
    resp_type: RespType,
    url: &str,
    redirected: bool,
    signal: Option<JSValue>,
) -> JSResult {
    let payload = transport_payload(tr, resp_type, url, redirected, signal);
    JSHttpClientResponse::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Constructor: new Response(body?, init?)
// ---------------------------------------------------------------------------

fn js_response_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    let mut synthetic: Option<Vec<u8>> = None;
    let mut ctor_stream: Option<JSReadableStream> = None;
    let mut status = 200u16;
    let mut status_text = String::new();
    let mut header_list = HeaderList::new();

    // Content-Type extracted from the body itself; an explicit `init.headers`
    // entry wins over it, so it is only applied after parsing init below.
    let mut ctor_content_type: Option<String> = None;

    // Init is parsed before the body so that an invalid status rejects even
    // when the body itself would also be invalid.
    if cc.length() > 1 {
        let init = cc.arg(1);
        if let Some(o) = init.as_object() {
            let s = o.get_property_str(ctx, "status")?;
            if !s.is_undefined() {
                let n = s.to_number(ctx)?.to_i32();
                if !(200..=599).contains(&n) {
                    return Err(ctx
                        .throw_range_error("Failed to construct 'Response': invalid status code"));
                }
                status = n as u16;
            }
            let st = o.get_property_str(ctx, "statusText")?;
            if !st.is_undefined() {
                let text = st.to_string(ctx)?.content().encode_utf8(false).to_string();
                // reason-phrase = *( HTAB / SP / VCHAR ), extended to the whole
                // Latin-1 range (0x80-0xFF) per the fetch spec's ByteString
                // statusText. 0x7F (DEL) and control chars (<= 0x1F) stay invalid.
                if !text.chars().all(|c| {
                    c == '\t'
                        || (' '..='\u{7E}').contains(&c)
                        || (c as u32) >= 0x80 && (c as u32) <= 0xFF
                }) {
                    return Err(
                        ctx.throw_type_error("Failed to construct 'Response': invalid status text")
                    );
                }
                status_text = text;
            }
            let mut h = o.get_property_str(ctx, "headers")?;
            if !h.is_undefined_or_null() {
                header_list = header_list_from_js(ctx, &mut h)?;
            }
        }
    }

    if cc.length() > 0 {
        let body_arg = cc.arg(0);
        if !body_arg.is_undefined_or_null() {
            // 204/205/304 are null-body statuses and cannot carry a body.
            if matches!(status, 204 | 205 | 304) {
                return Err(ctx.throw_type_error(
                    "Failed to construct 'Response': response with null body status cannot have body",
                ));
            }
            // A ReadableStream body must stay a stream; stringifying it would
            // silently produce "[object ReadableStream]".
            match JSReadableStream::try_from(body_arg) {
                Ok(s) => {
                    // A locked or already-read stream cannot become a body.
                    if s.payload()
                        .is_some_and(|p| p.reader.is_some() || p.disturbed)
                    {
                        return Err(ctx.throw_type_error(
                            "Failed to construct 'Response': body stream is locked or disturbed",
                        ));
                    }
                    ctor_stream = Some(s)
                }
                Err(_) => {
                    ctor_content_type = body_content_type(&body_arg);
                    synthetic = Some(body_to_bytes(ctx, &body_arg)?);
                }
            }
        }
    }
    if let Some(ct) = ctor_content_type
        && !header_list.has("content-type")
    {
        header_list.set("Content-Type", &ct);
    }

    JSResponse::new_instance(
        ctx,
        new_target,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: false,
            cached_body: None,
            synthetic_body: synthetic,
            ctor_stream,
            resp_type: RespType::Default,
            url: String::new(),
            redirected: false,
            status,
            status_text,
            content_type: String::new(),
            headers: Rc::new(RefCell::new(header_list)),
            content_length: None,
            http_version: "",
            signal: None,
            headers_obj: None,
            headers_guard: Guard::Response,
        },
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Statics: Response.error() / Response.redirect() / Response.json()
// ---------------------------------------------------------------------------

fn js_response_static_error(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: true,
            cached_body: None,
            synthetic_body: None,
            ctor_stream: None,
            resp_type: RespType::Error,
            url: String::new(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers: Rc::new(RefCell::new(HeaderList::new())),
            content_length: None,
            http_version: "",
            signal: None,
            headers_obj: None,
            headers_guard: Guard::Immutable,
        },
    )
    .map(JSValue::from)
}

fn js_response_static_redirect(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    if cc.length() < 1 {
        return Err(ctx.throw_type_error("Response.redirect requires a url"));
    }
    let url = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    // Per WHATWG fetch, redirect() parses the url against the current base URL
    // and only throws TypeError when that fails. A relative url like "/" is
    // valid whenever a base exists, so resolve it the way fetch() does.
    let url = match url::Url::parse(&url) {
        Ok(u) => u,
        Err(_) => ctx
            .current_module_name()
            .and_then(|base| url::Url::parse(&base).ok())
            .and_then(|base| base.join(&url).ok())
            .ok_or_else(|| {
                ctx.throw_type_error("Failed to execute 'Response.redirect': invalid url")
            })?,
    }
    .to_string();

    let status = if cc.length() > 1 {
        let s = cc.arg(1).to_number(ctx)?.to_i32();
        if ![301, 302, 303, 307, 308].contains(&s) {
            return Err(
                ctx.throw_range_error("Response.redirect status must be 301, 302, 303, 307 or 308")
            );
        }
        s as u16
    } else {
        302
    };

    let mut list = HeaderList::new();
    list.set("Location", &url);

    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            // A redirect response has a *null* body, not a consumed one:
            // `.body` must be null and consumers resolve to the empty value.
            body_consumed: false,
            cached_body: None,
            synthetic_body: None,
            ctor_stream: None,
            resp_type: RespType::Default,
            url: String::new(),
            redirected: false,
            status,
            status_text: String::new(),
            content_type: String::new(),
            headers: Rc::new(RefCell::new(list)),
            content_length: None,
            http_version: "",
            signal: None,
            headers_obj: None,
            headers_guard: Guard::Response,
        },
    )
    .map(JSValue::from)
}

/// `Response.json(data, init?)` — creates a JSON response.
fn js_response_static_json(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut status = 200u16;
    let mut status_text = String::new();
    let mut header_list = HeaderList::new();

    if cc.length() > 1
        && let Some(o) = cc.arg(1).as_object()
    {
        let s = o.get_property_str(ctx, "status")?;
        if !s.is_undefined() {
            let n = s.to_number(ctx)?.to_i32();
            if !(200..=599).contains(&n) {
                return Err(ctx.throw_range_error("Response.json: invalid status code"));
            }
            // Response.json always has a body, so null-body statuses are invalid.
            if matches!(n, 204 | 205 | 304) {
                return Err(ctx.throw_type_error(
                    "Response.json: response with null body status cannot have body",
                ));
            }
            status = n as u16;
        }

        let st = o.get_property_str(ctx, "statusText")?;
        if !st.is_undefined() {
            status_text = st.to_string(ctx)?.content().encode_utf8(false).to_string();
        }

        let mut h = o.get_property_str(ctx, "headers")?;
        if !h.is_undefined_or_null() {
            header_list = header_list_from_js(ctx, &mut h)?;
        }
    }

    // Use the engine's JSON.stringify so spec behavior is inherited: it throws
    // on circular structures, propagates errors from getters/toJSON, and
    // escapes lone surrogates as \uXXXX.
    let json_val = ctx.json_stringify(cc.arg(0), JSValue::UNDEFINED, JSValue::UNDEFINED)?;
    // stringify returns undefined for non-encodable input (symbol, function);
    // Response.json requires a body, so that is a TypeError.
    let Some(s) = json_val.as_string(false) else {
        return Err(ctx.throw_type_error("Response.json: data is not JSON-serializable"));
    };
    let json_str = s.content().encode_utf8(false).to_string();

    if !header_list.has("content-type") {
        header_list.set("Content-Type", "application/json");
    }

    let body = json_str.into_bytes();
    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: false,
            cached_body: None,
            synthetic_body: Some(body),
            ctor_stream: None,
            resp_type: RespType::Default,
            url: String::new(),
            redirected: false,
            status,
            status_text,
            content_type: "application/json".to_string(),
            headers: Rc::new(RefCell::new(header_list)),
            content_length: None,
            http_version: "",
            signal: None,
            headers_obj: None,
            headers_guard: Guard::Response,
        },
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Spec property getters
// ---------------------------------------------------------------------------

const RESPONSE_GET_TYPE: i16 = 0;
const RESPONSE_GET_URL: i16 = 1;
const RESPONSE_GET_REDIRECTED: i16 = 2;
const RESPONSE_GET_STATUS: i16 = 3;
const RESPONSE_GET_OK: i16 = 4;
const RESPONSE_GET_STATUS_TEXT: i16 = 5;
const RESPONSE_GET_CONTENT_TYPE: i16 = 6;
const RESPONSE_GET_CONTENT_LENGTH: i16 = 7;
const RESPONSE_GET_HEADERS: i16 = 8;
const RESPONSE_GET_BODY_USED: i16 = 9;
const RESPONSE_GET_HTTP_VERSION: i16 = 10;

fn js_response_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let kind = cc.magic() as i16;
    let this = cc.this();
    let p = get_response_payload(ctx, &this)?;

    match kind {
        RESPONSE_GET_TYPE => ctx
            .create_string_from_utf8(p.resp_type.as_str().as_bytes().to_vec())
            .map(JSValue::make_string),
        RESPONSE_GET_URL => ctx
            .create_string_from_utf8(p.url.as_bytes().to_vec())
            .map(JSValue::make_string),
        RESPONSE_GET_REDIRECTED => Ok(JSValue::from(p.redirected)),
        RESPONSE_GET_STATUS => Ok(JSValue::from(p.status as i32)),
        RESPONSE_GET_OK => Ok(JSValue::from(p.status >= 200 && p.status < 300)),
        RESPONSE_GET_STATUS_TEXT => ctx
            .create_string_from_utf8(p.status_text.as_bytes().to_vec())
            .map(JSValue::make_string),
        RESPONSE_GET_CONTENT_TYPE => ctx
            .create_string_from_utf8(p.content_type.as_bytes().to_vec())
            .map(JSValue::make_string),
        RESPONSE_GET_CONTENT_LENGTH => Ok(p
            .content_length
            .map(|v| JSValue::from(v as f64))
            .unwrap_or(JSValue::NULL)),
        RESPONSE_GET_HEADERS => {
            // [SameObject]: return the same Headers instance on every access.
            let mut this = cc.this();
            if let Some(h) = get_response_payload_mut(ctx, &mut this)?.headers_obj {
                return Ok(h);
            }
            let (shared, guard) = {
                let p = get_response_payload(ctx, &this)?;
                (p.headers.clone(), p.headers_guard)
            };
            // Share the payload's live header list so mutations through the
            // exposed `Headers` object propagate back to body consumers.
            let payload = HeadersPayload::from_shared(shared, guard);
            let headers = JSHeaders::new_instance(ctx, JSValue::UNDEFINED, payload)?;
            let hv = headers.into();
            get_response_payload_mut(ctx, &mut this)?.headers_obj = Some(hv);
            Ok(hv)
        }
        RESPONSE_GET_BODY_USED => {
            // Per fetch spec, bodyUsed is true if the body has been consumed OR
            // its underlying stream is disturbed. For a Response built from a
            // ReadableStream, reading the *original* stream directly (via its
            // own reader) must also mark bodyUsed.
            Ok(JSValue::from(p.body_consumed || p.body_disturbed()))
        }
        RESPONSE_GET_HTTP_VERSION => {
            // null for synthetic responses (no network transport involved).
            if p.http_version.is_empty() {
                Ok(JSValue::NULL)
            } else {
                ctx.create_string_from_utf8(p.http_version.as_bytes().to_vec())
                    .map(JSValue::make_string)
            }
        }
        _ => Err(ctx.throw_type_error("unknown Response getter")),
    }
}

fn js_response_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // error / opaqueredirect / opaque have no body per spec.
    if matches!(
        get_response_payload_mut(ctx, &mut this)?.resp_type,
        RespType::Error | RespType::OpaqueRedirect | RespType::Opaque
    ) {
        return Ok(JSValue::NULL);
    }

    // Return the same cached stream instance on every access.
    if let Some(s) = get_response_payload(ctx, &this)?.cached_body {
        return Ok(JSValue::from(s));
    }

    // A response constructed without a body has a null body per spec. This is
    // distinct from a consumed body, which stays non-null but disturbed.
    {
        let p = get_response_payload(ctx, &this)?;
        if !p.body_consumed
            && p.resp.is_none()
            && p.synthetic_body.is_none()
            && p.ctor_stream.is_none()
        {
            return Ok(JSValue::NULL);
        }
    }

    // First access (or access after consumption): build/disturb and cache.
    let stream = if get_response_payload_mut(ctx, &mut this)?.body_consumed {
        // A consumed body stays non-null but its getReader() must reject:
        // build an empty stream and lock it.
        let mut s = JSReadableStream::from_bytes(ctx, Vec::new())?;
        s.acquire_reader(ctx, false)?;
        s
    } else {
        // A constructor-supplied stream IS the body — expose it directly.
        if let Some(s) = get_response_payload_mut(ctx, &mut this)?.ctor_stream {
            get_response_payload_mut(ctx, &mut this)?.cached_body = Some(s);
            return Ok(JSValue::from(s));
        }
        let source = {
            let p = get_response_payload_mut(ctx, &mut this)?;
            if p.resp.is_some() || p.synthetic_body.is_some() {
                p.ensure_cached_body().ok()
            } else {
                None
            }
        };
        match source {
            Some(BodySource::Channel(rx)) => {
                let signal = get_response_payload(ctx, &this)?.signal;
                JSReadableStream::from_channel(ctx, rx, signal)?
            }
            Some(BodySource::Synthetic(bytes)) => JSReadableStream::from_bytes(ctx, bytes)?,
            // Handled above by the ctor_stream early return.
            Some(BodySource::Stream(s)) => s,
            None => {
                // No source: body stays non-null but getReader() must reject.
                let mut s = JSReadableStream::from_bytes(ctx, Vec::new())?;
                s.acquire_reader(ctx, false)?;
                s
            }
        }
    };

    get_response_payload_mut(ctx, &mut this)?.cached_body = Some(stream);
    Ok(JSValue::from(stream))
}

fn js_response_body_as(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let magic = cc.magic();
    let mut this = cc.this();

    let (source, html_convert) = {
        let p = get_response_payload_mut(ctx, &mut this)?;
        let is_html = p.content_type.starts_with("text/html");

        // Extended experimenal: the convert arg is only honored via BODY_AS_TEXT_CONVERT, which is
        // wired on HttpClientResponse.text(); base Response.text() ignores args.
        let convert = if magic == BODY_AS_TEXT_CONVERT && is_html && cc.length() > 0 {
            let arg0 = cc.arg(0);
            arg0.to_string(ctx)
                .ok()
                .and_then(|s| match s.content().encode_utf8(false).as_ref() {
                    "strip" => Some(HtmlConvertMode::Strip),
                    "markdown" => Some(HtmlConvertMode::Markdown),
                    _ => None,
                })
        } else {
            None
        };

        // Spec (Fetch): consuming a body whose signal is already aborted must
        // reject with the signal's reason (e.g. an "AbortError"), not resolve.
        if let Some(sig) = p.signal.as_ref()
            && let Some(reason) = signal_reason(sig)
        {
            return Promise::from_reject_value(ctx, reason).map(JSValue::from);
        }

        // Spec: consuming a body whose stream is already disturbed or locked
        // must reject with TypeError. The stream may be the constructor-supplied
        // one or the cached `.body` stream handed out earlier.
        let unusable = p.body_unusable();

        // An opaque / error / opaque-redirect response has a null body per the
        // Fetch spec: `text()`/etc. resolve to the empty value rather than
        // rejecting. These set `body_consumed: true` at construction, so they
        // would otherwise trip the "unusable" rejection below.
        let null_body_type = matches!(
            p.resp_type,
            RespType::Error | RespType::OpaqueRedirect | RespType::Opaque
        );

        if null_body_type {
            p.body_consumed = true;
            (Some(BodySource::Synthetic(Vec::new())), convert)
        } else if unusable {
            (None, convert)
        } else if p.resp.is_none()
            && p.synthetic_body.is_none()
            && p.ctor_stream.is_none()
            && p.cached_body.is_none()
        {
            // Null-body response (204/205/304, HEAD, or a synthetic response
            // built without a body that was never accessed as a stream):
            // consumers resolve to the empty value rather than rejecting.
            // `bodyUsed` is `body != null && body.stream.disturbed`, so a null
            // body stays undisturbed and repeated consumption keeps working.
            //
            // `cached_body` must be checked as well: the `.body` getter calls
            // ensure_cached_body(), which takes `resp` and moves the body into
            // the cached stream. Without it a network response whose `.body`
            // was merely touched would be misread as null-body and consumers
            // would resolve to "" instead of the real bytes.
            (Some(BodySource::Synthetic(Vec::new())), convert)
        } else {
            let src = if p.cached_body.is_some() && !p.body_consumed {
                // The `.body` getter already built the unique ReadableStream.
                // Per WHATWG fetch, body getters and consumers (text()/json()/
                // bytes()) must share that one stream; re-running
                // ensure_cached_body() would take the already-drained `resp`
                // and lose the body. Reuse the cached stream instance instead.
                p.cached_body.take().map(BodySource::Stream)
            } else {
                p.ensure_cached_body().ok()
            };

            // Mark body as consumed only when consuming via text/json/bytes/etc.,
            // NOT via the .body getter (js_response_body).
            if src.is_some() {
                p.body_consumed = true;
                // Lock the cached .body stream (if any) so its getReader()
                // throws. Skip the constructor-supplied stream: drain_body
                // acquires that reader itself.
                let is_own_stream =
                    matches!((p.cached_body, p.ctor_stream), (Some(a), Some(b)) if a == b);
                if let Some(mut s) = p.cached_body
                    && !is_own_stream
                {
                    // This closure returns a tuple (can't `?`); on a live
                    // cached body the lock won't fail, but if it does, drop
                    // the error rather than failing the consumer.
                    if s.acquire_reader(ctx, false).is_err() {
                        let _e = ctx.take_exception();
                        #[cfg(debug_assertions)]
                        eprintln!("[swallow-exc] response.rs:{}: {:?}", line!(), _e);
                    }
                }
            }

            (src, convert)
        }
    };

    // A null-body response (204/205/304/HEAD, or a synthetic response built
    // without a body that was never accessed as a stream) consumed as
    // `formData()` must reject per the Fetch spec when the `Content-Type` is
    // multipart/form-data — WPT "Consume response's body as formData with
    // correct multipart type (error case)". An `application/x-www-form-urlencoded`
    // null body still parses (to an empty FormData) and is allowed through.
    let is_null_body = {
        let p = JSHttpClientResponse::payload_of(&this).or_else(|| JSResponse::payload_of(&this));
        matches!(p, Some(p) if p.resp.is_none()
            && p.synthetic_body.is_none()
            && p.ctor_stream.is_none()
            && p.cached_body.is_none()
            && !matches!(p.resp_type, RespType::Error | RespType::OpaqueRedirect | RespType::Opaque))
    };
    if magic == BODY_AS_FORM_DATA && is_null_body {
        let p = JSHttpClientResponse::payload_of(&this).or_else(|| JSResponse::payload_of(&this));
        let ct = p
            .map(|p| p.headers.borrow().get("content-type").unwrap_or_default())
            .unwrap_or_default();
        if ct.to_ascii_lowercase().starts_with("multipart/form-data") {
            let _ = ctx.throw_type_error("cannot parse null body as FormData");
            let reason = match ctx.take_exception() {
                Exception::Error(v) => v,
                other => {
                    ctx.throw(other);
                    return Err(-1);
                }
            };
            return Promise::from_reject_value(ctx, reason).map(JSValue::from);
        }
    }

    // Response.blob() sets the Blob's type to the response's *current*
    // Content-Type header (which the user may have mutated via `headers.set`),
    // with any parameters (e.g. charset) stripped. Resolve it once before
    // borrowing `source`, since the payload borrow below lives in a nested scope.
    let blob_type = {
        let p = JSHttpClientResponse::payload_of(&this).or_else(|| JSResponse::payload_of(&this));
        match p {
            // formData() needs the *raw* Content-Type: the multipart boundary
            // lives in the parameters that blob_type_from_header strips.
            Some(p) if magic == BODY_AS_FORM_DATA => {
                p.headers.borrow().get("content-type").unwrap_or_default()
            }
            Some(p) => blob_type_from_header(&p.headers.borrow()),
            None => String::new(),
        }
    };

    match source {
        Some(source) => {
            // Normalize the convert variant back to plain text mode for draining.
            let mode = if magic == BODY_AS_TEXT_CONVERT {
                BODY_AS_TEXT
            } else {
                magic
            };
            drain_body(ctx, source, mode, html_convert, blob_type)
        }
        // Spec: these methods always return a promise; a disturbed/locked or
        // already-consumed body rejects it rather than throwing synchronously.
        None => {
            let _ = ctx.throw_type_error("body already consumed or unavailable");
            let reason = match ctx.take_exception() {
                Exception::Error(v) => v,
                // Non-Error exceptions (OOM, interrupt) must stay fatal.
                other => {
                    ctx.throw(other);
                    return Err(-1);
                }
            };
            Promise::from_reject_value(ctx, reason).map(JSValue::from)
        }
    }
}

/// `textStream()` — returns a `ReadableStream` of UTF-8-decoded string chunks.
/// Consumes the body (sets `bodyUsed`); a null-body response yields an empty
/// stream without marking the body used; a consumed or locked body throws.
fn js_response_text_stream(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let mut this = _cc.this();
    let source = {
        let p = get_response_payload_mut(ctx, &mut this)?;
        // Consumed or locked body: textStream throws TypeError.
        if p.body_unusable() {
            return Err(ctx.throw_type_error("body already consumed or locked"));
        }
        // Null-body response (never had a body): empty stream, bodyUsed stays
        // false, callable repeatedly.
        if p.resp.is_none() && p.synthetic_body.is_none() && p.ctor_stream.is_none() {
            None
        } else {
            let src = if p.cached_body.is_some() && !p.body_consumed {
                p.cached_body.take().map(BodySource::Stream)
            } else {
                p.ensure_cached_body().ok()
            };
            p.body_consumed = true;
            Some(src)
        }
    };
    match source {
        Some(Some(src)) => Ok(JSValue::from(JSReadableStream::from_text_source(ctx, src)?)),
        Some(None) => Err(ctx.throw_internal_error("failed to obtain body")),
        None => Ok(JSValue::from(JSReadableStream::from_text_source(
            ctx,
            BodySource::Synthetic(Vec::new()),
        )?)),
    }
}

fn js_response_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = get_response_payload_mut(ctx, &mut this)?;

    let _ = payload.body_rx.take();
    let _ = payload.resp.take();
    let _ = payload.synthetic_body.take();

    Ok(JSValue::UNDEFINED)
}

fn js_response_clone(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let (cloned, taken_resp) = {
        let p = get_response_payload_mut(ctx, &mut this)?;
        if p.body_consumed {
            (None, None)
        } else if p.resp.is_some() {
            let resp = p.resp.take();
            (Some(p), resp)
        } else {
            (Some(p), None)
        }
    };

    let Some(src) = cloned else {
        return Err(
            ctx.throw_type_error("Cannot clone a Response whose body has already been used")
        );
    };

    let synthetic = src.synthetic_body.clone();
    let payload = ResponsePayload {
        resp: taken_resp,
        body_rx: None,
        body_consumed: false,
        cached_body: None,
        synthetic_body: synthetic,
        // A stream body cannot be duplicated without teeing; leave it out.
        ctor_stream: None,
        resp_type: src.resp_type,
        url: src.url.clone(),
        redirected: src.redirected,
        status: src.status,
        status_text: src.status_text.clone(),
        content_type: src.content_type.clone(),
        // A cloned response owns an independent header set, not the shared one.
        headers: Rc::new(RefCell::new(src.headers.borrow().clone())),
        headers_obj: None,
        headers_guard: src.headers_guard,
        content_length: src.content_length,
        http_version: src.http_version,
        signal: src.signal,
    };
    // Preserve the concrete class: an HttpClientResponse clone stays HttpClientResponse.
    let this_ref = cc.this();
    if JSHttpClientResponse::payload_of(&this_ref).is_some() {
        JSHttpClientResponse::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
    } else {
        JSResponse::new_instance(ctx, cc.this(), payload).map(JSValue::from)
    }
}

// Strict WHATWG prototype — no aike extensions here (see HTTP_RESPONSE_PROTO).
const RESPONSE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("type", Some(js_response_getter), None, RESPONSE_GET_TYPE),
    FieldDef::GETSET("url", Some(js_response_getter), None, RESPONSE_GET_URL),
    FieldDef::GETSET(
        "redirected",
        Some(js_response_getter),
        None,
        RESPONSE_GET_REDIRECTED,
    ),
    FieldDef::GETSET(
        "status",
        Some(js_response_getter),
        None,
        RESPONSE_GET_STATUS,
    ),
    FieldDef::GETSET("ok", Some(js_response_getter), None, RESPONSE_GET_OK),
    FieldDef::GETSET(
        "statusText",
        Some(js_response_getter),
        None,
        RESPONSE_GET_STATUS_TEXT,
    ),
    FieldDef::GETSET(
        "headers",
        Some(js_response_getter),
        None,
        RESPONSE_GET_HEADERS,
    ),
    FieldDef::GETSET(
        "bodyUsed",
        Some(js_response_getter),
        None,
        RESPONSE_GET_BODY_USED,
    ),
    FieldDef::GETSET("body", Some(js_response_body), None, 0),
    FieldDef::FUNC("bytes", 0, js_response_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("text", 0, js_response_body_as, BODY_AS_TEXT as i16),
    FieldDef::FUNC("json", 0, js_response_body_as, BODY_AS_JSON as i16),
    FieldDef::FUNC(
        "arrayBuffer",
        0,
        js_response_body_as,
        BODY_AS_ARRAY_BUFFER as i16,
    ),
    FieldDef::FUNC("blob", 0, js_response_body_as, BODY_AS_BLOB as i16),
    FieldDef::FUNC("formData", 0, js_response_body_as, BODY_AS_FORM_DATA as i16),
    FieldDef::FUNC("textStream", 0, js_response_text_stream, 0),
    FieldDef::FUNC("clone", 0, js_response_clone, 0),
];

const RESPONSE_STATIC_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("error", 0, js_response_static_error, 0),
    FieldDef::FUNC("redirect", 2, js_response_static_redirect, 0),
    FieldDef::FUNC("json", 2, js_response_static_json, 0),
];

def_native_object!(
    "Response",
    JSResponse,
    ResponsePayload,
    RESPONSE_PROTO,
    Some(
        ClassCtorDef::new(js_response_constructor, 1, true)
            .entries(RESPONSE_STATIC_ENTRIES)
            .expose_to_global()
    ),
    on_payload_gc: ResponsePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSResponse::register(rt)
}

// ---------------------------------------------------------------------------
// HttpClientResponse — HttpClient-enhanced subclass of Response
// ---------------------------------------------------------------------------

fn js_http_response_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // Instances are only produced by HttpClient requests.
    Err(ctx.throw_type_error("HttpClientResponse is not constructable; use HttpClient"))
}

// Aike enhancements only; everything else is inherited from Response.
const HTTP_RESPONSE_PROTO: &[FieldDef] = &[
    // Shadows the base text(): accepts the optional html-convert arg.
    FieldDef::FUNC("text", 0, js_response_body_as, BODY_AS_TEXT_CONVERT as i16),
    FieldDef::GETSET(
        "contentType",
        Some(js_response_getter),
        None,
        RESPONSE_GET_CONTENT_TYPE,
    ),
    FieldDef::GETSET(
        "contentLength",
        Some(js_response_getter),
        None,
        RESPONSE_GET_CONTENT_LENGTH,
    ),
    FieldDef::GETSET(
        "httpVersion",
        Some(js_response_getter),
        None,
        RESPONSE_GET_HTTP_VERSION,
    ),
    FieldDef::FUNC("close", 0, js_response_close, 0),
];

def_native_object!(
    "HttpClientResponse",
    JSHttpClientResponse,
    ResponsePayload,
    HTTP_RESPONSE_PROTO,
    Some(ClassCtorDef::new(js_http_response_constructor, 0, false)),
    extends: JSResponse::class_id,
    on_payload_gc: ResponsePayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_client_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpClientResponse::register(rt)
}
