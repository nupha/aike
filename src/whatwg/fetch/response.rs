// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Fetch — `HttpResponse` class.
//!
//! This is the unified client-response class. The legacy `request()` global
//! and the new `fetch()` both return it, so the body-once consumption
//! semantics stay consistent across both APIs. It also exposes the WHATWG
//! `Response` constructor, `Response.error()` / `Response.redirect()`
//! statics, `clone()`, and the `Body` mixin.

use crossbeam::channel::Receiver;
use linkme::distributed_slice;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object,
};

use crate::{
    USER_CLASS_DEFS,
    html::HtmlConvertMode,
    whatwg::fetch::{
        body::{
            BODY_AS_BYTES, BODY_AS_JSON, BODY_AS_TEXT, body_to_bytes, drain_body,
            make_body_async_iterator,
        },
        headers::{HeaderList, HeadersPayload, JSHeaders, header_list_from_js},
        transport::TransportResponse,
    },
};

/// Payload backing a `Response` instance.
pub(crate) struct ResponsePayload {
    /// Open network response for lazy body streaming (None once taken).
    resp: Option<reqwest::Response>,
    /// Transient holder for the body channel while it is being consumed.
    body_rx: Option<Receiver<Vec<u8>>>,
    /// Whether the body has begun consumption (enforces single-use).
    body_consumed: bool,
    /// Fixed body bytes for constructed (synthetic) responses.
    synthetic_body: Option<Vec<u8>>,
    // --- WHATWG spec fields ---
    resp_type: String,
    url: String,
    redirected: bool,
    status: u16,
    status_text: String,
    content_type: String,
    headers: Vec<(String, String)>,
    content_length: Option<u64>,
}

impl ResponsePayload {
    /// Start body streaming (lazily) and return the `Receiver`.
    /// Returns `Err` if the body was already consumed or is unavailable.
    fn ensure_body_stream(&mut self) -> Result<Receiver<Vec<u8>>, i32> {
        if self.body_consumed {
            return Err(-1);
        }
        self.body_consumed = true;

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
            Ok(self.body_rx.take().unwrap())
        } else if let Some(bytes) = self.synthetic_body.take() {
            let (tx, rx) = crossbeam::channel::unbounded::<Vec<u8>>();
            self.body_rx = Some(rx);
            tokio::spawn(async move {
                let _ = tx.send(bytes);
            });
            Ok(self.body_rx.take().unwrap())
        } else {
            Err(-1)
        }
    }
}

fn get_response_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut ResponsePayload, i32> {
    val.as_native_object_mut::<JSResponse>()
        .and_then(|o| o.payload_mut())
        .ok_or_else(|| {
            ctx.throw_type_error("not an http response object");
            -1
        })
}

fn get_response_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a ResponsePayload, i32> {
    val.as_native_object::<JSResponse>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not an http response object");
            -1
        })
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
            synthetic_body: None,
            resp_type: "opaqueredirect".to_string(),
            url: String::new(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers: Vec::new(),
            content_length: None,
        },
    )
    .map(JSValue::from)
}

/// Build a `Response` instance from a [`TransportResponse`].
pub(crate) fn build_response_from_transport(
    ctx: &mut ScopedContext,
    tr: TransportResponse,
    resp_type: &str,
    url: &str,
    redirected: bool,
) -> JSResult {
    let resp_obj = JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: tr.resp,
            body_rx: None,
            body_consumed: false,
            synthetic_body: None,
            resp_type: resp_type.to_string(),
            url: url.to_string(),
            redirected,
            status: tr.status,
            status_text: tr.status_text,
            content_type: tr.content_type,
            headers: tr.headers,
            content_length: None,
        },
    )?;
    Ok(resp_obj.into())
}

// ---------------------------------------------------------------------------
// Constructor: new Response(body?, init?)
// ---------------------------------------------------------------------------

fn js_response_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    let mut synthetic: Option<Vec<u8>> = None;
    let mut status = 200u16;
    let mut status_text = String::new();
    let mut header_list = HeaderList::new();

    if cc.length() > 0 {
        let body_arg = cc.arg(0);
        if !body_arg.is_undefined_or_null() {
            synthetic = Some(body_to_bytes(ctx, &body_arg)?);
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
            synthetic_body: synthetic,
            resp_type: "default".to_string(),
            url: String::new(),
            redirected: false,
            status,
            status_text,
            content_type: String::new(),
            headers: header_list.to_vec(),
            content_length: None,
        },
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Statics: Response.error() / Response.redirect()
// ---------------------------------------------------------------------------

fn js_response_static_error(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    JSResponse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ResponsePayload {
            resp: None,
            body_rx: None,
            body_consumed: true, // body is null for error responses
            synthetic_body: None,
            resp_type: "error".to_string(),
            url: String::new(),
            redirected: false,
            status: 0,
            status_text: String::new(),
            content_type: String::new(),
            headers: Vec::new(),
            content_length: None,
        },
    )
    .map(JSValue::from)
}

fn js_response_static_redirect(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    if cc.length() < 1 {
        ctx.throw_type_error("Response.redirect requires a url");
        return Err(-1);
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
            ctx.throw_range_error("Response.redirect status must be 301, 302, 303, 307 or 308");
            return Err(-1);
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
            synthetic_body: None,
            resp_type: "default".to_string(),
            url: String::new(),
            redirected: false,
            status,
            status_text: String::new(),
            content_type: String::new(),
            headers: list.to_vec(),
            content_length: None,
        },
    )
    .map(JSValue::from)
}

/// Wire the static methods onto the `Response` constructor on `global`.
pub(crate) fn register_response_statics(
    ctx: &mut ScopedContext,
    global: &mut Object,
) -> Result<(), i32> {
    let ctor_val = global.get_property_str(ctx, "Response")?;
    let JSValue::Object(mut ctor) = ctor_val else {
        return Ok(());
    };

    let error_fn =
        ctx.create_native_function(Callback::Func(js_response_static_error), "error", 0, 0)?;
    ctor.define_property_value_str(ctx, "error", error_fn.into(), zuqe::JS_PROP_C_W_E)?;

    let redirect_fn = ctx.create_native_function(
        Callback::Func(js_response_static_redirect),
        "redirect",
        2,
        0,
    )?;
    ctor.define_property_value_str(ctx, "redirect", redirect_fn.into(), zuqe::JS_PROP_C_W_E)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Spec property getters
// ---------------------------------------------------------------------------

/// Magic constants distinguishing which field the shared `js_response_getter` returns.
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

/// Single getter for all `Response` data properties, dispatched by `cc.magic()`.
fn js_response_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let kind = cc.magic() as i16;
    let this = cc.this();
    let p = get_response_payload(ctx, &this)?;

    match kind {
        RESPONSE_GET_TYPE => {
            let s = ctx.create_string_from_utf8(p.resp_type.as_bytes().to_vec())?;
            Ok(s.into())
        }
        RESPONSE_GET_URL => {
            let s = ctx.create_string_from_utf8(p.url.as_bytes().to_vec())?;
            Ok(s.into())
        }
        RESPONSE_GET_REDIRECTED => Ok(JSValue::Bool(p.redirected)),
        RESPONSE_GET_STATUS => Ok(JSValue::from(p.status as i32)),
        RESPONSE_GET_OK => Ok(JSValue::Bool(p.status >= 200 && p.status < 300)),
        RESPONSE_GET_STATUS_TEXT => {
            let s = ctx.create_string_from_utf8(p.status_text.as_bytes().to_vec())?;
            Ok(s.into())
        }
        RESPONSE_GET_CONTENT_TYPE => {
            let s = ctx.create_string_from_utf8(p.content_type.as_bytes().to_vec())?;
            Ok(s.into())
        }
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
        RESPONSE_GET_BODY_USED => Ok(JSValue::Bool(p.body_consumed)),
        _ => {
            ctx.throw_type_error("unknown Response getter");
            Err(-1)
        }
    }
}

fn js_response_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let rx = {
        let p = get_response_payload_mut(ctx, &mut this)?;
        if p.resp.is_some() || p.synthetic_body.is_some() {
            match p.ensure_body_stream() {
                Ok(rx) => Some(rx),
                Err(_) => None,
            }
        } else if matches!(p.resp_type.as_str(), "error" | "opaque" | "opaqueredirect") {
            // null body
            return Ok(JSValue::NULL);
        } else {
            None
        }
    };

    match rx {
        Some(rx) => make_body_async_iterator(ctx, rx),
        None => {
            ctx.throw_type_error("body already consumed");
            Err(-1)
        }
    }
}

fn js_response_body_as(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let magic = cc.magic();

    let (rx, html_convert) = {
        let mut this = cc.this();
        let p = get_response_payload_mut(ctx, &mut this)?;
        let is_html = p.content_type.starts_with("text/html");
        let convert = if magic == BODY_AS_TEXT && is_html && cc.length() > 0 {
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

        if p.resp.is_none() && p.synthetic_body.is_none() {
            (None, convert)
        } else {
            (p.ensure_body_stream().ok(), convert)
        }
    };

    match rx {
        Some(rx) => drain_body(ctx, rx, magic, html_convert),
        None => {
            ctx.throw_type_error("body already consumed or unavailable");
            Err(-1)
        }
    }
}

fn js_response_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let data = get_response_payload_mut(ctx, &mut this)?;
    let _ = data.body_rx.take();
    let _ = data.resp.take();
    let _ = data.synthetic_body.take();
    Ok(JSValue::UNDEFINED)
}

fn js_response_clone(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let (cloned, taken_resp) = {
        let p = get_response_payload_mut(ctx, &mut this)?;
        if p.body_consumed {
            // body already locked — cannot clone
            (None, None)
        } else if p.resp.is_some() {
            // move the network body to the clone; original loses it
            let resp = p.resp.take();
            (Some(p), resp)
        } else {
            (Some(p), None)
        }
    };

    let Some(src) = cloned else {
        ctx.throw_type_error("Cannot clone a Response whose body has already been used");
        return Err(-1);
    };

    let synthetic = src.synthetic_body.clone();
    let payload = ResponsePayload {
        resp: taken_resp,
        body_rx: None,
        body_consumed: false,
        synthetic_body: synthetic,
        resp_type: src.resp_type.clone(),
        url: src.url.clone(),
        redirected: src.redirected,
        status: src.status,
        status_text: src.status_text.clone(),
        content_type: src.content_type.clone(),
        headers: src.headers.clone(),
        content_length: src.content_length,
    };
    JSResponse::new_instance(ctx, cc.this(), payload).map(JSValue::from)
}

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
    FieldDef::FUNC("clone", 0, js_response_clone, 0),
    FieldDef::FUNC("close", 0, js_response_close, 0),
];

def_native_object!(
    "Response",
    JSResponse,
    ResponsePayload,
    RESPONSE_PROTO,
    Some(ClassCtorDef::new(js_response_constructor, 1, true).expose_to_global())
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSResponse::register(rt)
}
