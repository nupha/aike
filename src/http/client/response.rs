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

use crossbeam::channel::Receiver;
use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, NativeObject, Runtime,
    ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    html::HtmlConvertMode,
    http::client::{
        body::{
            BODY_AS_BYTES, BODY_AS_JSON, BODY_AS_TEXT, BODY_AS_TEXT_CONVERT, BodySource,
            body_to_bytes, drain_body,
        },
        headers::{HeaderList, HeadersPayload, JSHeaders, header_list_from_js},
        transport::TransportResponse,
    },
    whatwg::stream::{
        JSReadableStream, call_opt_method, readable_stream_from_bytes, readable_stream_from_channel,
    },
};

/// WHATWG `Response.type` (closed set of values this runtime produces).
/// `Error` / `OpaqueRedirect` force `body` to null per spec.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RespType {
    Basic,
    Default,
    Error,
    OpaqueRedirect,
}

impl RespType {
    /// Spec string exposed via the `type` getter.
    fn as_str(self) -> &'static str {
        match self {
            RespType::Basic => "basic",
            RespType::Default => "default",
            RespType::Error => "error",
            RespType::OpaqueRedirect => "opaqueredirect",
        }
    }
}

pub(crate) struct ResponsePayload {
    /// Open network response for lazy body streaming (None once taken).
    resp: Option<reqwest::Response>,
    /// Transient holder for the body channel while it is being consumed.
    body_rx: Option<Receiver<Vec<u8>>>,
    /// Whether the body has begun consumption (enforces single-use).
    body_consumed: bool,
    /// Cached `ReadableStream` returned by `response.body` (same instance each
    /// access; locked once the body is consumed).
    body_stream: Option<JSValue>,
    /// Fixed body bytes for constructed (synthetic) responses.
    synthetic_body: Option<Vec<u8>>,
    /// `ReadableStream` passed to `new Response(stream)`; drained on consumption.
    stream_body: Option<JSReadableStream>,

    /// WHATWG `Response.type`; see [`RespType`].
    resp_type: RespType,

    url: String,
    redirected: bool,
    status: u16,
    status_text: String,
    content_type: String,
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
    /// Negotiated HTTP version ("1.1", "2", …); empty for synthetic responses.
    http_version: &'static str,
}

impl ResponsePayload {
    /// Keep JS-visible stream handles alive across GC while the body is held.
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.body_stream {
            s.trace(gcx);
        }
        if let Some(s) = self.stream_body {
            JSValue::from(s).trace(gcx);
        }
    }

    /// Start body streaming (lazily) and return a `BodySource`.
    /// Returns `Err` if the body was already consumed or is unavailable.
    fn ensure_body_stream(&mut self) -> Result<BodySource, i32> {
        if self.body_consumed {
            return Err(-1);
        }

        if let Some(resp) = self.resp.take() {
            let (tx, rx) = crossbeam::channel::unbounded::<Vec<u8>>();
            self.body_rx = Some(rx);

            tokio::spawn(async move {
                use futures_util::StreamExt;

                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if tx.send(bytes.to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tx.send(format!("{e}").into_bytes()).ok();
                            break;
                        }
                    }
                }
            });
            Ok(BodySource::Channel(self.body_rx.clone().unwrap()))
        } else if let Some(bytes) = self.synthetic_body.clone() {
            Ok(BodySource::Synthetic(bytes))
        } else if let Some(stream) = self.stream_body {
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
pub(crate) fn build_opaque_redirect_response(ctx: &mut ScopedContext) -> JSResult {
    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: true,
            body_stream: None,
            synthetic_body: None,
            stream_body: None,
            resp_type: RespType::OpaqueRedirect,
            url: String::new(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers: Vec::new(),
            content_length: None,
            http_version: "",
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
) -> ResponsePayload {
    ResponsePayload {
        resp: tr.resp,
        body_rx: None,
        body_consumed: false,
        body_stream: None,
        synthetic_body: None,
        stream_body: None,
        resp_type,
        url: url.to_string(),
        redirected,
        status: tr.status,
        status_text: tr.status_text,
        content_type: tr.content_type,
        headers: tr.headers,
        content_length: tr.content_length,
        http_version: tr.http_version,
    }
}

/// Build a plain WHATWG `Response` from a [`TransportResponse`] (fetch path).
pub(crate) fn build_response_from_transport(
    ctx: &mut ScopedContext,
    tr: TransportResponse,
    resp_type: RespType,
    url: &str,
    redirected: bool,
) -> JSResult {
    let payload = transport_payload(tr, resp_type, url, redirected);
    JSResponse::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

/// Build an enhanced `HttpClientResponse` from a [`TransportResponse`] (HttpClient path).
pub(crate) fn build_http_response_from_transport(
    ctx: &mut ScopedContext,
    tr: TransportResponse,
    resp_type: RespType,
    url: &str,
    redirected: bool,
) -> JSResult {
    let payload = transport_payload(tr, resp_type, url, redirected);
    JSHttpClientResponse::new_instance(ctx, JSValue::UNDEFINED, payload).map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Constructor: new Response(body?, init?)
// ---------------------------------------------------------------------------

fn js_response_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    let mut synthetic: Option<Vec<u8>> = None;
    let mut stream_body: Option<JSReadableStream> = None;
    let mut status = 200u16;
    let mut status_text = String::new();
    let mut header_list = HeaderList::new();

    if cc.length() > 0 {
        let body_arg = cc.arg(0);
        if !body_arg.is_undefined_or_null() {
            // A ReadableStream body must stay a stream; stringifying it would
            // silently produce "[object ReadableStream]".
            match JSReadableStream::try_from(body_arg) {
                Ok(s) => stream_body = Some(s),
                Err(_) => synthetic = Some(body_to_bytes(ctx, &body_arg)?),
            }
        }
    }
    if cc.length() > 1 {
        let init = cc.arg(1);
        if let Some(o) = init.as_object() {
            let s = o.get_property_str(ctx, "status")?;
            if !s.is_undefined() {
                status = s.to_number(ctx)?.to_i32() as u16;
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
    }

    JSResponse::new_instance(
        ctx,
        new_target,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: false,
            body_stream: None,
            synthetic_body: synthetic,
            stream_body,
            resp_type: RespType::Default,
            url: String::new(),
            redirected: false,
            status,
            status_text,
            content_type: String::new(),
            headers: header_list.to_vec(),
            content_length: None,
            http_version: "",
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
            body_stream: None,
            synthetic_body: None,
            stream_body: None,
            resp_type: RespType::Error,
            url: String::new(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers: Vec::new(),
            content_length: None,
            http_version: "",
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
            body_consumed: true,
            body_stream: None,
            synthetic_body: None,
            stream_body: None,
            resp_type: RespType::Default,
            url: String::new(),
            redirected: false,
            status,
            status_text: String::new(),
            content_type: String::new(),
            headers: list.to_vec(),
            content_length: None,
            http_version: "",
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
            status = s.to_number(ctx)?.to_i32() as u16;
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

    let json_str = match cc.arg(0).to_serde_json(ctx) {
        Ok(val) => serde_json::to_string(&val).unwrap_or_else(|_| "null".to_string()),
        Err(_) => "null".to_string(),
    };

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
            body_stream: None,
            synthetic_body: Some(body),
            stream_body: None,
            resp_type: RespType::Default,
            url: String::new(),
            redirected: false,
            status,
            status_text,
            content_type: "application/json".to_string(),
            headers: header_list.to_vec(),
            content_length: None,
            http_version: "",
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
            let headers = JSHeaders::new_instance(
                ctx,
                JSValue::UNDEFINED,
                HeadersPayload {
                    list: HeaderList::from_vec(p.headers.clone()),
                },
            )?;
            Ok(headers.into())
        }
        RESPONSE_GET_BODY_USED => Ok(JSValue::from(p.body_consumed)),
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

/// Build a ReadableStream already locked/disturbed, so any later
/// getReader() call throws a TypeError. Used as the `body` value once the body
/// has been consumed (spec: response.body stays non-null, but body.getReader()
/// must reject).
fn disturbed_body_stream(ctx: &mut ScopedContext) -> JSResult {
    let stream = readable_stream_from_bytes(ctx, Vec::new())?;
    let _ = call_opt_method(ctx, stream, "getReader", &[]);
    Ok(stream)
}

fn js_response_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // error / opaqueredirect have no body per spec.
    if matches!(
        get_response_payload_mut(ctx, &mut this)?.resp_type,
        RespType::Error | RespType::OpaqueRedirect
    ) {
        return Ok(JSValue::NULL);
    }

    // Return the same cached stream instance on every access.
    {
        let p = get_response_payload_mut(ctx, &mut this)?;
        if let Some(s) = p.body_stream {
            return Ok(s);
        }
    }

    // First access (or access after consumption): build/disturb and cache.
    let stream = if get_response_payload_mut(ctx, &mut this)?.body_consumed {
        disturbed_body_stream(ctx)?
    } else {
        // A constructor-supplied stream IS the body — expose it directly.
        if let Some(s) = get_response_payload_mut(ctx, &mut this)?.stream_body {
            let s = JSValue::from(s);
            get_response_payload_mut(ctx, &mut this)?.body_stream = Some(s);
            return Ok(s);
        }
        let source = {
            let p = get_response_payload_mut(ctx, &mut this)?;
            if p.resp.is_some() || p.synthetic_body.is_some() {
                p.ensure_body_stream().ok()
            } else {
                None
            }
        };
        match source {
            Some(BodySource::Channel(rx)) => readable_stream_from_channel(ctx, rx)?,
            Some(BodySource::Synthetic(bytes)) => readable_stream_from_bytes(ctx, bytes)?,
            // Handled above by the stream_body early return.
            Some(BodySource::Stream(s)) => s.into(),
            None => disturbed_body_stream(ctx)?,
        }
    };

    get_response_payload_mut(ctx, &mut this)?.body_stream = Some(stream);
    Ok(stream)
}

fn js_response_body_as(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let magic = cc.magic();

    let (source, html_convert) = {
        let mut this = cc.this();
        let p = get_response_payload_mut(ctx, &mut this)?;
        let is_html = p.content_type.starts_with("text/html");
        // The convert arg is only honored via BODY_AS_TEXT_CONVERT, which is
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

        if p.resp.is_none() && p.synthetic_body.is_none() && p.stream_body.is_none() {
            (None, convert)
        } else {
            let src = p.ensure_body_stream().ok();
            // Mark body as consumed only when consuming via text/json/bytes/etc.,
            // NOT via the .body getter (js_response_body).
            if src.is_some() {
                p.body_consumed = true;
                // Lock the cached .body stream (if any) so its getReader() throws.
                // Skip when it is the constructor-supplied stream: drain_body
                // acquires that reader itself.
                let is_own_stream =
                    matches!((p.body_stream, p.stream_body), (Some(a), Some(b)) if a == b.into());
                if let Some(s) = p.body_stream
                    && !is_own_stream
                {
                    let _ = call_opt_method(ctx, s, "getReader", &[]);
                }
            }
            (src, convert)
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
            drain_body(ctx, source, mode, html_convert)
        }
        None => Err(ctx.throw_type_error("body already consumed or unavailable")),
    }
}

fn js_response_form_data(ctx: &mut ScopedContext, _: CallContext) -> JSResult {
    Err(ctx.throw_type_error("formData() is not supported in this runtime"))
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
        body_stream: None,
        synthetic_body: synthetic,
        // A stream body cannot be duplicated without teeing; leave it out.
        stream_body: None,
        resp_type: src.resp_type,
        url: src.url.clone(),
        redirected: src.redirected,
        status: src.status,
        status_text: src.status_text.clone(),
        content_type: src.content_type.clone(),
        headers: src.headers.clone(),
        content_length: src.content_length,
        http_version: src.http_version,
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
    FieldDef::FUNC("arrayBuffer", 0, js_response_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("blob", 0, js_response_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("formData", 0, js_response_form_data, 0),
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
    extends: JSResponse::class_id
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_client_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpClientResponse::register(rt)
}
