// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use linkme::distributed_slice;
use tokio::sync::{mpsc, oneshot};
use zuqe::{
    CallContext, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, Runtime, ScopedContext,
    builtins::json::JSON,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::outbound_buffer::{OUTBOUND_CAP, OutboundBuffer, js_stream_write};

/// Response data sent back from JS to the axum handler.
pub(crate) enum HttpResponseMessage {
    /// Normal one-shot response (status + headers + optional body).
    Normal {
        status: u16,
        headers: Vec<(String, String)>,
        body: Option<String>,
    },
    /// SSE streaming response: headers + a receiver the axum `Sse` reads from.
    Streaming {
        status: u16,
        headers: Vec<(String, String)>,
        receiver: mpsc::Receiver<String>,
    },
}

/// Internal payload for HttpResponse classes.
pub(crate) struct HttpResponsePayload {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Option<String>,
    pub(crate) ended: bool,
    /// Set once `sse()` is called — every other method then throws, because
    /// the original response object is consumed and replaced by an
    /// `HttpResponseSse` instance.
    pub(crate) switched: bool,
    pub(crate) response_tx: Option<oneshot::Sender<HttpResponseMessage>>,
    /// Shared close-callback list from the request.
    pub(crate) close_cbs: super::CloseCbs,
}

impl HttpResponsePayload {
    /// Send the response via the oneshot channel, consuming the sender.
    pub(crate) fn send_response(&mut self) {
        if let Some(tx) = self.response_tx.take() {
            let msg = HttpResponseMessage::Normal {
                status: self.status,
                headers: std::mem::take(&mut self.headers),
                body: self.body.take(),
            };
            let _ = tx.send(msg);
        }
    }
}

pub(super) fn get_res_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HttpResponsePayload, i32> {
    if let Some(p) = JSHttpResponse::payload_mut_of(val) {
        if p.switched {
            Err(ctx.throw_type_error("response already switched to SSE"))
        } else {
            Ok(p)
        }
    } else {
        Err(ctx.throw_internal_error("HttpResponse was detached"))
    }
}

fn js_res_status(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let code_raw = if cc.length() > 0 {
        cc.arg(0).to_number(ctx).ok().and_then(|n| match n {
            IntOrFloat::Int64(v) => Some(v as u16),
            _ => None,
        })
    } else {
        None
    };
    let data = get_res_payload_mut(ctx, &mut this)?;
    if let Some(c) = code_raw {
        data.status = c;
    }
    Ok(this)
}

fn js_res_setHeader(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
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
    let data = get_res_payload_mut(ctx, &mut this)?;
    data.headers.push((key, val));
    Ok(this)
}

fn js_res_setHeaders(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    // Collect headers from object first (before borrowing payload)
    let headers = if cc.length() > 0 {
        if let Some(mut obj) = cc.arg(0).as_object() {
            let pks = obj
                .get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK)
                .ok();
            let mut entries = Vec::new();
            if let Some(keys) = pks {
                for pk in keys.iter() {
                    if let Ok(key_v) = pk.atom.to_value(ctx)
                        && let Ok(key_r) = key_v.to_string(ctx)
                    {
                        let key_str = key_r.content().encode_utf8(false).to_string();
                        if let Ok(val) = obj.get_property(ctx, pk.atom)
                            && let Ok(val_r) = val.to_string(ctx)
                        {
                            entries.push((key_str, val_r.content().encode_utf8(false).to_string()));
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
    let data = get_res_payload_mut(ctx, &mut this)?;
    data.headers.extend(headers);
    Ok(this)
}

const FINISH_END: i32 = 0;
const FINISH_JSON: i32 = 1;
const FINISH_SEND_STATUS: i32 = 2;
const FINISH_REDIRECT: i32 = 3;

fn js_res_finish(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let magic = cc.magic();

    let body_str;
    let status_code;
    let redirect_url;
    match magic {
        FINISH_END => {
            body_str = if cc.length() > 0 && !cc.arg(0).is_undefined() {
                cc.arg(0)
                    .to_string(ctx)
                    .ok()
                    .map(|s| s.content().encode_utf8(false).to_string())
            } else {
                None
            };
            status_code = None;
            redirect_url = None;
        }
        FINISH_JSON => {
            // Use the engine's JSON.stringify so spec behavior is inherited: it
            // throws on circular structures, propagates errors from
            // getters/toJSON, and returns undefined for non-encodable input.
            body_str = if cc.length() > 0 {
                let result =
                    JSON::stringify(ctx, cc.arg(0), JSValue::UNDEFINED, JSValue::UNDEFINED)?;
                result
                    .to_string(ctx)
                    .ok()
                    .map(|s| s.content().encode_utf8(false).to_string())
            } else {
                None
            };
            status_code = None;
            redirect_url = None;
        }
        FINISH_SEND_STATUS => {
            body_str = None;
            status_code = if cc.length() > 0 {
                cc.arg(0).to_number(ctx).ok().and_then(|n| match n {
                    IntOrFloat::Int64(v) => Some(v as u16),
                    _ => None,
                })
            } else {
                None
            };
            redirect_url = None;
        }
        FINISH_REDIRECT => {
            let c = if cc.length() > 0 {
                cc.arg(0)
                    .to_number(ctx)
                    .ok()
                    .and_then(|n| match n {
                        IntOrFloat::Int64(v) => Some(v as u16),
                        _ => None,
                    })
                    .unwrap_or(302)
            } else {
                302
            };
            let u = if cc.length() >= 2 {
                cc.arg(1)
                    .to_string(ctx)
                    .ok()
                    .map(|s| s.content().encode_utf8(false).to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            body_str = None;
            status_code = Some(c);
            redirect_url = Some(u);
        }
        _ => unreachable!(),
    }

    let data = get_res_payload_mut(ctx, &mut this)?;
    if data.ended {
        return Ok(JSValue::UNDEFINED);
    }

    if magic == FINISH_JSON {
        let has_ct = data
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
        if !has_ct {
            data.headers
                .push(("content-type".into(), "application/json".into()));
        }
    }
    if let Some(url) = redirect_url {
        data.headers.push(("Location".into(), url));
    }

    if let Some(b) = body_str {
        data.body = Some(b);
    }
    if let Some(c) = status_code {
        data.status = c;
    }
    data.ended = true;
    data.send_response();
    Ok(JSValue::UNDEFINED)
}

const RES_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("status", 1, js_res_status, 0),
    FieldDef::FUNC("setHeader", 2, js_res_setHeader, 0),
    FieldDef::FUNC("setHeaders", 1, js_res_setHeaders, 0),
    FieldDef::FUNC("end", 1, js_res_finish, FINISH_END as _),
    FieldDef::FUNC("json", 1, js_res_finish, FINISH_JSON as _),
    FieldDef::FUNC("sendStatus", 1, js_res_finish, FINISH_SEND_STATUS as _),
    FieldDef::FUNC("redirect", 2, js_res_finish, FINISH_REDIRECT as _),
    FieldDef::FUNC("sse", 0, js_res_sse, 0),
];

impl HttpResponsePayload {
    /// GC trace: keep close-callback JS values reachable.
    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        for v in self.close_cbs.borrow().iter() {
            zuqe::gc_lite::GcTrace::trace(v, gcx);
        }
    }
}

impl GcTrace for HttpResponsePayload {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        self.gc_trace(gcx);
    }
}

def_native_object!(
    "HttpServerResponse",
    JSHttpResponse,
    HttpResponsePayload,
    RES_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpResponse::register(rt)
}

// ---------------------------------------------------------------------------
// SSE response (`HttpResponseSse`) — independent streaming object returned by
// `res.sse()`. Only exposes write/flush/close; the original `HttpServerResponse`
// is consumed (payload invalidated) so json/setHeader/end can no longer be used.
// ---------------------------------------------------------------------------

/// SSE-only payload: a write-side `OutboundBuffer` plus a closed flag.
/// Holds no status/headers/body — those were already sent when `sse()` opened
/// the streaming response.
pub(crate) struct HttpResponseSsePayload {
    pub(crate) sink: crate::http::server::outbound_buffer::OutboundBuffer,
    pub(crate) closed: bool,
    /// Shared close-callback list — fired on first channel-close (client disconnect).
    pub(crate) close_cbs: super::CloseCbs,
}

impl HttpResponseSsePayload {
    /// GC trace callback wired via `def_native_object!`'s `on_payload_gc`.
    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.sink.trace(gcx);
        // Trace close-callback JS values so they stay reachable.
        for v in self.close_cbs.borrow().iter() {
            zuqe::gc_lite::GcTrace::trace(v, gcx);
        }
    }
}

impl GcTrace for HttpResponseSsePayload {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        self.gc_trace(gcx);
    }
}

pub(crate) fn get_sse_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HttpResponseSsePayload, i32> {
    JSHttpResponseSse::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not an HttpResponseSse instance"))
}

/// Append SSE-default headers if not already set by the user.
fn ensure_sse_headers(headers: &mut Vec<(String, String)>) {
    let has_ct = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    if !has_ct {
        headers.push(("content-type".into(), "text/event-stream".into()));
    }
    let has_cc = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("cache-control"));
    if !has_cc {
        headers.push(("cache-control".into(), "no-cache".into()));
    }
    let has_accel = headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-accel-buffering"));
    if !has_accel {
        headers.push(("x-accel-buffering".into(), "no".into()));
    }
}

/// `res.sse()` — open an SSE streaming response and return an `HttpResponseSse`.
fn js_res_sse(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let data = get_res_payload_mut(ctx, &mut this)?;

    // Already switched → error.
    if data.response_tx.is_none() {
        return Err(ctx.throw_type_error("response already switched to SSE"));
    }

    let mut headers = std::mem::take(&mut data.headers);
    ensure_sse_headers(&mut headers);
    let status = data.status;

    // Build the shared write-side buffer; its receiver drives the axum Sse.
    let (mut sink, rx) = OutboundBuffer::new(OUTBOUND_CAP);
    // Wire disconnect callbacks so req.onClose(cb) fires when the client leaves.
    sink.set_close_cbs(data.close_cbs.clone());

    // Open the streaming response immediately (before JS writes the first frame).
    let msg = HttpResponseMessage::Streaming {
        status,
        headers,
        receiver: rx,
    };
    if let Some(tx) = data.response_tx.take() {
        let _ = tx.send(msg);
    }

    // Invalidate the original response object.
    data.switched = true;
    data.ended = true;

    let sse = JSHttpResponseSse::new_instance(
        ctx,
        JSValue::UNDEFINED,
        HttpResponseSsePayload {
            sink,
            closed: false,
            close_cbs: data.close_cbs.clone(),
        },
    )?;
    Ok(sse.into())
}

fn js_sse_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let frame = cc.arg(0).to_string(ctx)?;
    let frame = frame.content().encode_utf8(false).to_string();
    let mut this = cc.this();
    let p = get_sse_payload(ctx, &mut this)?;
    if p.closed {
        return Ok(JSValue::UNDEFINED);
    }
    let ret = js_stream_write(&mut p.sink, ctx, frame)?;
    Ok(ret.unwrap_or(JSValue::UNDEFINED))
}

fn js_sse_flush(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // Frames are pushed to the bounded channel immediately; the axum `Sse`
    // consumer reads them as they arrive. Nothing to flush on our side.
    Ok(JSValue::UNDEFINED)
}

fn js_sse_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_sse_payload(ctx, &mut this)?;
    if !p.closed {
        p.closed = true;
        p.sink.shutdown(); // drop sender → Sse stream ends
    }
    Ok(JSValue::UNDEFINED)
}

/// `sse.onClose(cb)` — register a callback for client disconnect (same list as req.onClose).
fn js_sse_on_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_sse_payload(ctx, &mut this)?;
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

const SSE_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("write", 1, js_sse_write, 0),
    FieldDef::FUNC("flush", 0, js_sse_flush, 0),
    FieldDef::FUNC("close", 0, js_sse_close, 0),
    FieldDef::FUNC("onClose", 1, js_sse_on_close, 0),
];

def_native_object!(
    "HttpResponseSse",
    JSHttpResponseSse,
    HttpResponseSsePayload,
    SSE_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_http_response_sse(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHttpResponseSse::register(rt)
}
