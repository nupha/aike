// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::RefCell;
use std::rc::Rc;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, Exception, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext, def_native_object,
};

use crate::{
    USER_CLASS_DEFS,
    http::client::{
        body::{
            BODY_AS_BLOB, BODY_AS_BYTES, BODY_AS_JSON, BODY_AS_TEXT, BodySource,
            blob_type_from_header, body_content_type, body_to_bytes, drain_body,
            text_stream_from_source,
        },
        headers::{Guard, HeaderList, HeadersPayload, JSHeaders, header_list_from_js},
    },
    whatwg::{
        abort::{JSAbortSignal, make_default_signal, signal_attach_follower},
        stream::{
            JSReadableStream, make_rejected_promise, readable_stream_from_bytes,
            readable_stream_from_channel,
        },
    },
};

pub(crate) struct RequestPayload {
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) headers: Rc<RefCell<HeaderList>>,
    pub(crate) destination: String,
    pub(crate) referrer: String,
    pub(crate) referrer_policy: String,
    pub(crate) mode: String,
    pub(crate) credentials: String,
    pub(crate) cache: String,
    pub(crate) redirect: String,
    pub(crate) integrity: String,
    pub(crate) keepalive: bool,
    pub(crate) signal: Option<JSValue>,
    pub(crate) synthetic_body: Option<Vec<u8>>,
    /// A `ReadableStream` body, kept unread until the request is sent.
    pub(crate) stream_body: Option<JSValue>,
    pub(crate) body_consumed: bool,
}

fn get_request_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut RequestPayload, i32> {
    JSRequest::payload_mut_of(val).ok_or_else(|| ctx.throw_type_error("not a Request instance"))
}

fn get_request_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a RequestPayload, i32> {
    JSRequest::payload_of(val).ok_or_else(|| ctx.throw_type_error("not a Request instance"))
}

fn js_request_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let input = cc.arg(0);

    let mut url_str = String::new();
    let mut method = String::from("GET");
    let mut header_list = HeaderList::new();
    let mut synthetic_body: Option<Vec<u8>> = None;
    let mut stream_body: Option<JSValue> = None;
    let mut redirect = String::from("follow");
    let mut signal: Option<JSValue> = None;

    // Inherit from a Request input.
    if let Some(p) = JSRequest::payload_of(&input) {
        url_str = p.url.clone();
        method = p.method.clone();
        header_list = p.headers.borrow().clone();
        redirect = p.redirect.clone();
        signal = p.signal;
        synthetic_body = p.synthetic_body.clone();
        stream_body = p.stream_body;
    } else if !input.is_undefined_or_null() {
        let href = input.get_property_str(ctx, "href")?;
        if !href.is_undefined() {
            url_str = href
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
        } else {
            url_str = input
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
        }
    }

    // Apply RequestInit overrides.
    if cc.length() > 1 {
        let init = cc.arg(1);
        if let Some(o) = init.as_object() {
            let m = o.get_property_str(ctx, "method")?;
            if !m.is_undefined() {
                method = m
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string()
                    .to_uppercase();
            }
            let mut h = o.get_property_str(ctx, "headers")?;
            if !h.is_undefined_or_null() {
                header_list = header_list_from_js(ctx, &mut h)?;
            }
            // `duplex` must be read off the init dictionary whenever a body is
            // present, and read before `body` so a getter on it still fires.
            let duplex = o.get_property_str(ctx, "duplex")?;
            let duplex = match duplex.is_undefined() {
                true => None,
                false => Some(
                    duplex
                        .to_string(ctx)?
                        .content()
                        .encode_utf8(false)
                        .to_string(),
                ),
            };

            let b = o.get_property_str(ctx, "body")?;
            if !b.is_undefined_or_null() {
                // The body source implies a default Content-Type unless the
                // caller already supplied one.
                if let Some(t) = body_content_type(&b)
                    && !header_list.has("content-type")
                {
                    header_list.set("Content-Type", &t);
                }

                match JSReadableStream::payload_of(&b).is_some() {
                    // A stream body requires `duplex: "half"`; without it the
                    // request is rejected rather than silently buffered.
                    true if duplex.as_deref() != Some("half") => {
                        return Err(ctx.throw_type_error(
                            "Request body is a ReadableStream but duplex is not 'half'",
                        ));
                    }
                    // The stream stays unread here: it is the request's body and
                    // is only pumped when the request is actually sent.
                    true => stream_body = Some(b.clone()),
                    false => synthetic_body = Some(body_to_bytes(ctx, &b)?),
                }
            }
            let r = o.get_property_str(ctx, "redirect")?;
            if !r.is_undefined() {
                redirect = r.to_string(ctx)?.content().encode_utf8(false).to_string();
            }
            let s = o.get_property_str(ctx, "signal")?;
            if !s.is_undefined_or_null() && s.as_native_object::<JSAbortSignal>().is_some() {
                signal = Some(s.clone());
            }
        }
    }

    // WHATWG: `request.signal` is a fresh "follow" signal that follows the
    // supplied signal (`request.signal !== signal`), and is never the same
    // object. If no signal is supplied, create a default non-aborted signal so
    // that `request.signal` is always present.
    let signal = match signal {
        Some(parent) => {
            let child = make_default_signal(ctx)?;
            signal_attach_follower(&parent, &child);
            Some(child)
        }
        None => Some(make_default_signal(ctx)?),
    };

    let mut payload = RequestPayload {
        method,
        url: url_str,
        headers: Rc::new(RefCell::new(header_list)),
        destination: String::new(),
        referrer: "about:client".to_string(),
        referrer_policy: String::new(),
        mode: "cors".to_string(),
        credentials: "same-origin".to_string(),
        cache: "default".to_string(),
        redirect,
        integrity: String::new(),
        keepalive: false,
        signal,
        synthetic_body,
        stream_body,
        body_consumed: false,
    };

    // Override remaining RequestInit string fields if provided.
    if cc.length() > 1 {
        let init = cc.arg(1);
        if let Some(o) = init.as_object() {
            let mut str_field = |o: zuqe::Object, name: &str, default: &str| -> String {
                // safe defaulting helper (ignores errors)
                match o.get_property_str(ctx, name) {
                    Ok(v) if !v.is_undefined() => v
                        .to_string(ctx)
                        .map(|s| s.content().encode_utf8(false).to_string())
                        .unwrap_or_else(|_| default.to_string()),
                    _ => default.to_string(),
                }
            };
            payload.destination = str_field(o, "destination", "");
            payload.referrer = str_field(o, "referrer", "about:client");
            payload.referrer_policy = str_field(o, "referrerPolicy", "");
            payload.mode = str_field(o, "mode", "cors");
            payload.credentials = str_field(o, "credentials", "same-origin");
            payload.cache = str_field(o, "cache", "default");
            payload.integrity = str_field(o, "integrity", "");
            let ka = o.get_property_str(ctx, "keepalive")?;
            if !ka.is_undefined() {
                payload.keepalive = ka
                    .to_number(ctx)
                    .map(|n| n.to_f64() != 0.0)
                    .unwrap_or(false);
            }
        }
    }

    JSRequest::new_instance(ctx, new_target, payload).map(JSValue::from)
}

// --- getters ---

macro_rules! req_str_getter {
    ($name:ident, $field:ident) => {
        fn $name(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
            let this = cc.this();
            let p = get_request_payload(ctx, &this)?;
            ctx.create_string_from_utf8(p.$field.as_bytes().to_vec())
                .map(JSValue::make_string)
        }
    };
}

req_str_getter!(js_req_method, method);
req_str_getter!(js_req_url, url);
req_str_getter!(js_req_destination, destination);
req_str_getter!(js_req_referrer, referrer);
req_str_getter!(js_req_referrer_policy, referrer_policy);
req_str_getter!(js_req_mode, mode);
req_str_getter!(js_req_credentials, credentials);
req_str_getter!(js_req_cache, cache);
req_str_getter!(js_req_redirect, redirect);
req_str_getter!(js_req_integrity, integrity);

fn js_req_keepalive_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_request_payload(ctx, &this)?;
    Ok(JSValue::from(p.keepalive))
}

fn js_req_signal_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_request_payload(ctx, &this)?;
    Ok(p.signal.unwrap_or(JSValue::NULL))
}

fn js_req_headers_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_request_payload(ctx, &this)?;
    // Share the payload's live header list so mutations through the exposed
    // `Headers` object are visible to the request (Fetch spec [SameObject]).
    let guard = if p.mode == "no-cors" {
        Guard::RequestNoCors
    } else {
        Guard::None
    };
    let payload = HeadersPayload::from_shared(p.headers.clone(), guard);
    let headers = JSHeaders::new_instance(ctx, JSValue::UNDEFINED, payload)?;
    Ok(headers.into())
}

fn js_req_body_used_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_request_payload(ctx, &this)?;
    Ok(JSValue::from(p.body_consumed))
}

fn js_req_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // A stream body is exposed as-is: it is the very stream the caller passed
    // in, so `request.body` must hand back that same object.
    {
        let p = get_request_payload_mut(ctx, &mut this)?;
        if !p.body_consumed
            && let Some(s) = p.stream_body
        {
            return Ok(s);
        }
    }

    let source = {
        let p = get_request_payload_mut(ctx, &mut this)?;
        if p.body_consumed {
            None
        } else if let Some(bytes) = p.synthetic_body.take() {
            p.body_consumed = true;
            Some(BodySource::Synthetic(bytes))
        } else {
            None
        }
    };
    match source {
        Some(BodySource::Channel(rx)) => {
            // Request bodies are buffered into plain `Vec<u8>` chunks; wrap them
            // as `Ok` so they flow through the error-aware channel stream.
            let (tx, rx2) = crossbeam::channel::unbounded::<Result<Vec<u8>, String>>();
            std::thread::spawn(move || {
                while let Ok(b) = rx.recv() {
                    // `b` already carries the chunk/error result; forward as-is.
                    if tx.send(b).is_err() {
                        break;
                    }
                }
            });
            readable_stream_from_channel(ctx, rx2)
        }
        Some(BodySource::Synthetic(bytes)) => readable_stream_from_bytes(ctx, bytes),
        // Request bodies are always buffered, never a live stream.
        Some(BodySource::Stream(s)) => Ok(s.into()),
        None => Ok(JSValue::NULL),
    }
}

/// Body read methods for Request (bytes, text, json, arrayBuffer).
fn js_req_body_as(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let magic = cc.magic();

    let mut this = cc.this();
    let source = {
        let p = get_request_payload_mut(ctx, &mut this)?;
        if p.body_consumed {
            None
        } else if let Some(bytes) = p.synthetic_body.take() {
            p.body_consumed = true;
            Some(BodySource::Synthetic(bytes))
        } else {
            // Null-body request (e.g. `new Request(url)` with no init body):
            // consumers resolve to the empty value rather than rejecting.
            p.body_consumed = true;
            Some(BodySource::Synthetic(Vec::new()))
        }
    };

    // Request.blob() sets the Blob's type to the request's *current*
    // Content-Type header (mutable via `headers.set`), with parameters
    // stripped. Computed before consuming the source below.
    let blob_type = if magic == BODY_AS_BLOB {
        let p = get_request_payload(ctx, &this)?;
        blob_type_from_header(&p.headers.borrow())
    } else {
        String::new()
    };

    match source {
        Some(source) => drain_body(ctx, source, magic, None, blob_type),
        // Spec: these methods always return a promise; an already-consumed body
        // rejects it rather than throwing synchronously.
        None => {
            let _ = ctx.throw_type_error("body already consumed or unavailable");
            let reason = match ctx.take_exception() {
                Exception::Error(v) => v,
                other => {
                    ctx.throw(other);
                    return Err(-1);
                }
            };
            make_rejected_promise(ctx, reason)
        }
    }
}

/// `textStream()` — returns a `ReadableStream` of UTF-8-decoded string chunks.
/// Consumes the body (sets `bodyUsed`); a null-body request yields an empty
/// stream without marking the body used, and an already-consumed body throws.
fn js_req_text_stream(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let mut this = _cc.this();
    let source = {
        let p = get_request_payload_mut(ctx, &mut this)?;
        if p.body_consumed {
            return Err(ctx.throw_type_error("body already consumed or locked"));
        }
        // Null-body request: empty stream, bodyUsed stays false, reusable.
        if p.synthetic_body.is_none() && p.stream_body.is_none() {
            None
        } else if let Some(bytes) = p.synthetic_body.take() {
            p.body_consumed = true;
            Some(BodySource::Synthetic(bytes))
        } else {
            p.body_consumed = true;
            let s = p.stream_body.take().expect("stream_body checked above");
            Some(BodySource::Stream(JSReadableStream::try_from(s)?))
        }
    };
    match source {
        Some(src) => text_stream_from_source(ctx, src),
        None => text_stream_from_source(ctx, BodySource::Synthetic(Vec::new())),
    }
}

/// `formData()` — not yet supported.
fn js_req_form_data(ctx: &mut ScopedContext, _: CallContext) -> JSResult {
    Err(ctx.throw_type_error("formData() is not supported in this runtime"))
}

fn js_req_clone(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    let src = get_request_payload(ctx, &this)?;
    if src.body_consumed {
        return Err(ctx.throw_type_error("Cannot clone a Request whose body has already been used"));
    }

    let payload = RequestPayload {
        method: src.method.clone(),
        url: src.url.clone(),
        // A cloned request owns an independent header set, not the shared one.
        headers: Rc::new(RefCell::new(src.headers.borrow().clone())),
        destination: src.destination.clone(),
        referrer: src.referrer.clone(),
        referrer_policy: src.referrer_policy.clone(),
        mode: src.mode.clone(),
        credentials: src.credentials.clone(),
        cache: src.cache.clone(),
        redirect: src.redirect.clone(),
        integrity: src.integrity.clone(),
        keepalive: src.keepalive,
        signal: src.signal,
        synthetic_body: src.synthetic_body.clone(),
        stream_body: src.stream_body,
        body_consumed: false,
    };
    JSRequest::new_instance(ctx, this, payload).map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

const REQUEST_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("method", Some(js_req_method), None, 0),
    FieldDef::GETSET("url", Some(js_req_url), None, 0),
    FieldDef::GETSET("destination", Some(js_req_destination), None, 0),
    FieldDef::GETSET("referrer", Some(js_req_referrer), None, 0),
    FieldDef::GETSET("referrerPolicy", Some(js_req_referrer_policy), None, 0),
    FieldDef::GETSET("mode", Some(js_req_mode), None, 0),
    FieldDef::GETSET("credentials", Some(js_req_credentials), None, 0),
    FieldDef::GETSET("cache", Some(js_req_cache), None, 0),
    FieldDef::GETSET("redirect", Some(js_req_redirect), None, 0),
    FieldDef::GETSET("integrity", Some(js_req_integrity), None, 0),
    FieldDef::GETSET("keepalive", Some(js_req_keepalive_getter), None, 0),
    FieldDef::GETSET("signal", Some(js_req_signal_getter), None, 0),
    FieldDef::GETSET("headers", Some(js_req_headers_getter), None, 0),
    FieldDef::GETSET("bodyUsed", Some(js_req_body_used_getter), None, 0),
    FieldDef::GETSET("body", Some(js_req_body), None, 0),
    FieldDef::FUNC("bytes", 0, js_req_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("arrayBuffer", 0, js_req_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("text", 0, js_req_body_as, BODY_AS_TEXT as i16),
    FieldDef::FUNC("json", 0, js_req_body_as, BODY_AS_JSON as i16),
    FieldDef::FUNC("blob", 0, js_req_body_as, BODY_AS_BLOB as i16),
    FieldDef::FUNC("formData", 0, js_req_form_data, 0),
    FieldDef::FUNC("textStream", 0, js_req_text_stream, 0),
    FieldDef::FUNC("clone", 0, js_req_clone, 0),
];

def_native_object!(
    "Request",
    JSRequest,
    RequestPayload,
    REQUEST_PROTO,
    Some(ClassCtorDef::new(js_request_constructor, 1, true).expose_to_global())
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_request(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSRequest::register(rt)
}
