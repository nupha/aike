// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;
use std::collections::HashMap;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassID, FieldDef, JSResult, JSValue, NativeObject, Runtime, ScopedContext,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::Method;

/// Internal payload for HttpRequest classes.
pub(super) struct HttpRequestPayload {
    pub(super) method: Method,
    pub(super) path: String,
    pub(super) params: HashMap<String, String>,
    pub(super) query: HashMap<String, String>,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Option<Vec<u8>>,
}

fn get_req_payload<'a>(
    ctx: &'a mut ScopedContext,
    val: &JSValue,
) -> Result<&'a mut HttpRequestPayload, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSHttpRequest::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("HttpRequest was disposed");
        Err(-1)
    } else {
        Ok(unsafe { &mut *opaque.cast::<HttpRequestPayload>() })
    }
}

fn js_req_method(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let method = {
        let data = get_req_payload(ctx, &this)?;
        data.method.as_str()
    };
    let s = ctx.create_string_from_utf8(method.as_bytes().to_vec())?;
    Ok(s.into())
}

fn js_req_path(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let path = {
        let data = get_req_payload(ctx, &this)?;
        data.path.clone()
    };
    let s = ctx.create_string_from_utf8(path.as_bytes().to_vec())?;
    Ok(s.into())
}

fn js_req_params(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let params = {
        let data = get_req_payload(ctx, &this)?;
        data.params.clone()
    };
    let mut obj = ctx.create_base_object()?;
    for (k, v) in &params {
        let val = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        obj.set_property_str(ctx, k, val.into())?;
    }
    Ok(obj.into())
}

fn js_req_query(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let query = {
        let data = get_req_payload(ctx, &this)?;
        data.query.clone()
    };
    let mut obj = ctx.create_base_object()?;
    for (k, v) in &query {
        let val = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        obj.set_property_str(ctx, k, val.into())?;
    }
    Ok(obj.into())
}

fn js_req_headers(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let headers = {
        let data = get_req_payload(ctx, &this)?;
        data.headers.clone()
    };
    let mut obj = ctx.create_base_object()?;
    for (k, v) in &headers {
        let val = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        obj.set_property_str(ctx, k, val.into())?;
    }
    Ok(obj.into())
}

fn js_req_json(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let body = {
        let data = get_req_payload(ctx, &this)?;
        data.body.take().unwrap_or_default()
    };
    if body.is_empty() {
        return Ok(JSValue::UNDEFINED);
    }
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        ctx.throw_type_error(&format!("invalid JSON body: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &value)
}

fn js_req_text(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let body = {
        let data = get_req_payload(ctx, &this)?;
        data.body.take().unwrap_or_default()
    };
    let s = ctx.create_string_from_utf8(body)?;
    Ok(s.into())
}

fn js_req_bytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let body = {
        let data = get_req_payload(ctx, &this)?;
        data.body.take().unwrap_or_default()
    };
    zuqe::builtins::typed_array::TypedArray::create_from_vec_u8(ctx, body).map(JSValue::from)
}

const REQ_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("method", Some(js_req_method), None, 0),
    FieldDef::GETSET("path", Some(js_req_path), None, 0),
    FieldDef::GETSET("params", Some(js_req_params), None, 0),
    FieldDef::GETSET("query", Some(js_req_query), None, 0),
    FieldDef::GETSET("headers", Some(js_req_headers), None, 0),
    FieldDef::FUNC("json", 0, js_req_json, 0),
    FieldDef::FUNC("text", 0, js_req_text, 0),
    FieldDef::FUNC("bytes", 0, js_req_bytes, 0),
];

def_native_object!(
    "HttpServerRequest",
    JSHttpRequest,
    Box<HttpRequestPayload>,
    REQ_PROTO
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_request(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpRequest::register(rt)
}
