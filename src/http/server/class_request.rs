// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use zuqe::{
    CallContext, FieldDef, JSResult, JSValue, ScopedContext,
    builtins::{json::JSON, promise::Promise},
    def_native_object,
    gc_lite::GcTrace,
};

use super::super::Method;

/// Shared close-callback list: stored in both HttpRequest and HttpResponse;
/// the SSE payload clones the Rc when `res.sse()` is called, and fires
/// the callbacks when the underlying channel closes (client disconnect).
pub(crate) type CloseCbs = Rc<RefCell<Vec<JSValue>>>;

/// Internal payload for HttpRequest classes.
pub(super) struct HttpRequestPayload {
    pub(super) method: Method,
    pub(super) path: String,
    pub(super) params: HashMap<String, String>,
    pub(super) query: HashMap<String, String>,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Option<Vec<u8>>,
    /// Shared with HttpResponse/SSE — fired on client disconnect.
    pub(super) close_cbs: CloseCbs,
}

fn gc_trace_req(data: &HttpRequestPayload, gcx: &mut zuqe::gc_lite::GcTraceCtx) {
    for v in data.close_cbs.borrow().iter() {
        zuqe::gc_lite::GcTrace::trace(v, gcx);
    }
}

impl GcTrace for HttpRequestPayload {
    fn trace(&self, gcx: &mut zuqe::gc_lite::GcTraceCtx) {
        gc_trace_req(self, gcx);
    }
}

fn get_req_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a HttpRequestPayload, i32> {
    JSHttpRequest::payload_of(val)
        .ok_or_else(|| ctx.throw_type_error("not an HttpRequest instance"))
}

fn get_req_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HttpRequestPayload, i32> {
    JSHttpRequest::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not an HttpRequest instance"))
}

fn js_req_method(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let method = {
        let this = cc.this();
        get_req_payload(ctx, &this)?.method
    };
    ctx.create_string_from_utf8(method.as_str().as_bytes().to_vec())
        .map(JSValue::make_string)
}

fn js_req_path(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = {
        let this = cc.this();
        let p = get_req_payload(ctx, &this)?;
        p.path.as_bytes().to_vec()
    };
    ctx.create_string_from_utf8(path).map(JSValue::make_string)
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
        obj.set_property_str(ctx, k, JSValue::make_string(val))?;
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
        obj.set_property_str(ctx, k, JSValue::make_string(val))?;
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
        obj.set_property_str(ctx, k, JSValue::make_string(val))?;
    }
    Ok(obj.into())
}

const BODY_JSON: i32 = 0;
const BODY_TEXT: i32 = 1;
const BODY_BYTES: i32 = 2;

fn js_req_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let body = get_req_payload_mut(ctx, &mut cc.this())?
        .body
        .take()
        .unwrap_or_default();

    let value = match cc.magic() {
        BODY_JSON => {
            if body.is_empty() {
                JSValue::UNDEFINED
            } else {
                let text = String::from_utf8(body)
                    .map_err(|e| ctx.throw_type_error(&format!("invalid UTF-8 body: {e}")))?;
                JSON::parse(ctx, &text)?
            }
        }
        BODY_TEXT => ctx
            .create_string_from_utf8(body)
            .map(JSValue::make_string)?,
        BODY_BYTES => zuqe::builtins::typed_array::TypedArray::create_from_vec_u8(ctx, body)
            .map(JSValue::from)?,
        _ => unreachable!(),
    };

    Promise::from_resolve_value(ctx, value).map(JSValue::from)
}

/// `req.onClose(cb)` — register a callback fired when the client disconnects.
fn js_req_on_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_req_payload_mut(ctx, &mut this)?;
    if cc.length() < 1 {
        return Err(ctx.throw_type_error("`onClose` requires a callback"));
    }
    let cb = cc.arg(0);
    if !cb.is_callable() {
        return Err(ctx.throw_type_error("`onClose` argument must be a function"));
    }
    p.close_cbs.borrow_mut().push(cb);
    Ok(JSValue::UNDEFINED)
}

const REQ_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("method", Some(js_req_method), None, 0),
    FieldDef::GETSET("path", Some(js_req_path), None, 0),
    FieldDef::GETSET("params", Some(js_req_params), None, 0),
    FieldDef::GETSET("query", Some(js_req_query), None, 0),
    FieldDef::GETSET("headers", Some(js_req_headers), None, 0),
    FieldDef::FUNC("json", 0, js_req_body, BODY_JSON as _),
    FieldDef::FUNC("text", 0, js_req_body, BODY_TEXT as _),
    FieldDef::FUNC("bytes", 0, js_req_body, BODY_BYTES as _),
    FieldDef::FUNC("onClose", 1, js_req_on_close, 0),
];

def_native_object!(
    "HttpServerRequest",
    JSHttpRequest,
    HttpRequestPayload,
    REQ_PROTO,
    None
);
