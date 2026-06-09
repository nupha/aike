// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;

use linkme::distributed_slice;
use tokio::sync::oneshot;
use zuqe::{
    CallContext, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, NativeObject, Runtime,
    ScopedContext, def_native_object,
};

use crate::USER_CLASS_DEFS;

/// Response data sent back from JS to the axum handler.
#[derive(Debug)]
pub(crate) struct HttpResponseMessage {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Option<String>,
}

/// Internal payload for HttpResponse classes.
pub(crate) struct HttpResponsePayload {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Option<String>,
    pub(crate) ended: bool,
    pub(crate) response_tx: Option<oneshot::Sender<HttpResponseMessage>>,
}

impl HttpResponsePayload {
    /// Send the response via the oneshot channel, consuming the sender.
    pub(crate) fn send_response(&mut self) {
        if let Some(tx) = self.response_tx.take() {
            let msg = HttpResponseMessage {
                status: self.status,
                headers: std::mem::take(&mut self.headers),
                body: self.body.take(),
            };
            let _ = tx.send(msg);
        }
    }
}

pub(crate) fn get_res_payload<'a>(
    ctx: &'a mut ScopedContext,
    val: &JSValue,
) -> Result<&'a mut HttpResponsePayload, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSHttpResponse::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_internal_error("HttpResponse was disposed");
        Err(-1)
    } else {
        Ok(unsafe { &mut *opaque.cast::<HttpResponsePayload>() })
    }
}

fn js_res_status(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let code_raw = if cc.length() > 0 {
        cc.arg(0).to_number(ctx).ok().and_then(|n| match n {
            IntOrFloat::Int64(v) => Some(v as u16),
            _ => None,
        })
    } else {
        None
    };
    let data = get_res_payload(ctx, &this)?;
    if let Some(c) = code_raw {
        data.status = c;
    }
    Ok(this)
}

fn js_res_setHeader(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let (key, val) = if cc.length() >= 2 {
        let k = cc.arg(0).to_string(ctx)?;
        let v = cc.arg(1).to_string(ctx)?;
        (
            k.content().encode_utf8(false).to_string(),
            v.content().encode_utf8(false).to_string(),
        )
    } else {
        return Ok(this);
    };
    let data = get_res_payload(ctx, &this)?;
    data.headers.push((key, val));
    Ok(this)
}

fn js_res_setHeaders(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    // Collect headers from object first (before borrowing payload)
    let headers = if cc.length() > 0 {
        if let Some(mut obj) = cc.arg(0).as_object().cloned() {
            let pks = obj
                .get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_INDEX_MASK)
                .ok();
            let mut entries = Vec::new();
            if let Some(keys) = pks {
                for pk in keys.iter() {
                    if let Ok(key_v) = pk.atom.to_value(ctx) {
                        if let Ok(key_r) = key_v.to_string(ctx) {
                            let key_str = key_r.content().encode_utf8(false).to_string();
                            if let Ok(val) = obj.get_property(ctx, pk.atom) {
                                if let Ok(val_r) = val.to_string(ctx) {
                                    entries.push((
                                        key_str,
                                        val_r.content().encode_utf8(false).to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            entries
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Now borrow payload and extend headers
    let data = get_res_payload(ctx, &this)?;
    data.headers.extend(headers);
    Ok(this)
}

fn js_res_end(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let body_str = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0)
            .to_string(ctx)
            .ok()
            .map(|s| s.content().encode_utf8(false).to_string())
    } else {
        None
    };
    let data = get_res_payload(ctx, &this)?;
    if data.ended {
        return Ok(JSValue::UNDEFINED);
    }
    if let Some(b) = body_str {
        data.body = Some(b);
    }
    data.ended = true;
    data.send_response();
    Ok(JSValue::UNDEFINED)
}

fn js_res_json(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let json_str = if cc.length() > 0 {
        // Serialize via JS JSON.stringify
        let global = ctx.global_obj();
        let json_val = global.get_property_str(ctx, "JSON")?;
        let stringify = json_val.get_property_str(ctx, "stringify")?;
        let result = match stringify.call(ctx, json_val, &[cc.arg(0)]) {
            Ok(r) => r,
            Err(_) => {
                ctx.throw_type_error("JSON.stringify failed on the provided value");
                return Err(-1);
            }
        };
        result
            .to_string(ctx)
            .ok()
            .map(|s| s.content().encode_utf8(false).to_string())
    } else {
        None
    };
    let data = get_res_payload(ctx, &this)?;
    if data.ended {
        return Ok(JSValue::UNDEFINED);
    }
    let has_ct = data
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    if !has_ct {
        data.headers
            .push(("content-type".into(), "application/json".into()));
    }
    if let Some(s) = json_str {
        data.body = Some(s);
    }
    data.ended = true;
    data.send_response();
    Ok(JSValue::UNDEFINED)
}

fn js_res_sendStatus(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let code = if cc.length() > 0 {
        cc.arg(0).to_number(ctx).ok().and_then(|n| match n {
            IntOrFloat::Int64(v) => Some(v as u16),
            _ => None,
        })
    } else {
        None
    };
    let data = get_res_payload(ctx, &this)?;
    if data.ended {
        return Ok(JSValue::UNDEFINED);
    }
    if let Some(c) = code {
        data.status = c;
    }
    data.ended = true;
    data.send_response();
    Ok(JSValue::UNDEFINED)
}

fn js_res_redirect(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let (code, url_str) = if cc.length() >= 2 {
        let c = cc
            .arg(0)
            .to_number(ctx)
            .ok()
            .and_then(|n| match n {
                IntOrFloat::Int64(v) => Some(v as u16),
                _ => None,
            })
            .unwrap_or(302);
        let u = cc
            .arg(1)
            .to_string(ctx)
            .ok()
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();
        (c, u)
    } else {
        (302, String::new())
    };
    let data = get_res_payload(ctx, &this)?;
    if data.ended {
        return Ok(JSValue::UNDEFINED);
    }
    data.status = code;
    data.headers.push(("Location".into(), url_str));
    data.ended = true;
    data.send_response();
    Ok(JSValue::UNDEFINED)
}

const RES_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("status", 1, js_res_status, 0),
    FieldDef::FUNC("setHeader", 2, js_res_setHeader, 0),
    FieldDef::FUNC("setHeaders", 1, js_res_setHeaders, 0),
    FieldDef::FUNC("end", 1, js_res_end, 0),
    FieldDef::FUNC("json", 1, js_res_json, 0),
    FieldDef::FUNC("sendStatus", 1, js_res_sendStatus, 0),
    FieldDef::FUNC("redirect", 2, js_res_redirect, 0),
];

def_native_object!(
    "HttpServerResponse",
    JSHttpResponse,
    Box<HttpResponsePayload>,
    RES_PROTO
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpResponse::register(rt)
}
