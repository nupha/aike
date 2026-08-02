// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, Exception, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext, def_native_object,
};

use crate::{
    USER_CLASS_DEFS,
    http::client::{
        body::{BODY_AS_BYTES, BODY_AS_JSON, BODY_AS_TEXT, BodySource, body_to_bytes, drain_body},
        headers::{HeaderList, HeadersPayload, JSHeaders, header_list_from_js},
    },
    whatwg::{
        abort::JSAbortSignal,
        stream::{make_rejected_promise, readable_stream_from_bytes, readable_stream_from_channel},
    },
};

pub(crate) struct RequestPayload {
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) headers: HeaderList,
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
    let mut redirect = String::from("follow");
    let mut signal: Option<JSValue> = None;

    // Inherit from a Request input.
    if let Some(p) = JSRequest::payload_of(&input) {
        url_str = p.url.clone();
        method = p.method.clone();
        header_list = p.headers.clone();
        redirect = p.redirect.clone();
        signal = p.signal;
        synthetic_body = p.synthetic_body.clone();
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
            let b = o.get_property_str(ctx, "body")?;
            if !b.is_undefined_or_null() {
                synthetic_body = Some(body_to_bytes(ctx, &b)?);
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

    let mut payload = RequestPayload {
        method,
        url: url_str,
        headers: header_list,
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
    let list = p.headers.clone();
    // A `no-cors` request exposes headers with the "request-no-cors" guard.
    let payload = if p.mode == "no-cors" {
        HeadersPayload::no_cors(list)
    } else {
        HeadersPayload::new(list)
    };
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

    match source {
        Some(source) => drain_body(ctx, source, magic, None),
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
        headers: src.headers.clone(),
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
    FieldDef::FUNC("blob", 0, js_req_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("formData", 0, js_req_form_data, 0),
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
