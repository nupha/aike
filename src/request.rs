// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, time::Duration};

use crossbeam::channel;
use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
};

use crate::USER_CLASS_DEFS;

struct ResponseData {
    body_rx: Option<crossbeam::channel::Receiver<Vec<u8>>>,
}

impl ResponseData {
    fn take_body_rx(&mut self) -> Option<crossbeam::channel::Receiver<Vec<u8>>> {
        self.body_rx.take()
    }
}

/// Get a `&mut ResponseData` from a Response instance's opaque pointer.
fn get_response_data<'a>(
    ctx: &'a mut ScopedContext,
    val: &JSValue,
) -> Result<&'a mut ResponseData, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSResponse::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("Response was disposed");
        Err(-1)
    } else {
        Ok(unsafe { &mut *opaque.cast::<ResponseData>() })
    }
}

// ---------------------------------------------------------------------------
// Transient metadata sent through the Promise slot (not opaque-stored)
// ---------------------------------------------------------------------------

struct ResponseMeta {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    content_type: String,
    content_length: Option<u64>,
}

/// request(url, options?)  –  returns Promise<HttpResponse>
pub(crate) fn js_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let url_str = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    // parse options
    let mut method = String::from("GET");
    let mut req_headers: Vec<(String, String)> = Vec::new();
    let mut req_body: Option<String> = None;
    let mut timeout: Option<Duration> = None;

    if cc.length() > 1 {
        let arg1 = cc.arg(1);
        if let Some(opts) = arg1.as_object() {
            // method
            let v = opts.get_property_str(ctx, "method")?;
            if !v.is_undefined() {
                method = v
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string()
                    .to_uppercase();
            }

            // headers
            let mut v = opts.get_property_str(ctx, "headers")?;
            if let Some(hdr_obj) = v.as_object_mut() {
                let pks = hdr_obj.get_own_property_names(
                    ctx,
                    zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_INDEX_MASK,
                )?;
                for pk in pks.iter() {
                    let key_v = pk.atom.to_value(ctx)?;
                    let key_r = key_v.to_string(ctx)?;
                    let key_str = key_r.content().encode_utf8(false).to_string();
                    let val = hdr_obj.get_property(ctx, pk.atom)?;
                    req_headers.push((
                        key_str,
                        val.to_string(ctx)?.content().encode_utf8(false).to_string(),
                    ));
                }
            }

            // body
            let v = opts.get_property_str(ctx, "body")?;
            if !v.is_undefined_or_null() {
                let s = v.to_string(ctx)?;
                req_body = Some(s.content().encode_utf8(false).to_string());
            }

            // timeout (milliseconds) — None = no timeout per reqwest default
            let timeout_ms = opts.get_property_str(ctx, "timeout")?;
            if !timeout_ms.is_undefined() {
                timeout.replace(
                    timeout_ms
                        .to_number(ctx)
                        .map(|n| Duration::from_millis(n.to_u32() as u64))?,
                );
            }
        }
    }

    let (body_tx, body_rx) = channel::bounded::<Vec<u8>>(8);

    //
    // Promise that resolves when reqwest headers arrive.
    // The async task builds ResponseMeta (status, headers, etc.) and calls
    // result_slot.commit().  This closure receives the meta, builds the
    // Response class instance (without streaming body), and resolves.
    // The body AsyncIterableIterator is created lazily via a GETSET on proto.
    //
    let result_slot = PromiseResultSlot::<Result<ResponseMeta, String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, payload| -> JSResult {
            //
            // build Response instance with ResponseMeta committed by reqwest async task
            //
            let meta = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

            // create opaque data (body_rx stored for lazy body / text / json)
            let data = Box::new(ResponseData {
                body_rx: Some(body_rx),
            });
            let opaque = Box::into_raw(data).cast::<c_void>() as *mut c_void;

            // create Response instance
            let mut resp_obj = cx
                .create_class_instance_with_opaque(
                    JSValue::UNDEFINED,
                    JSResponse::class_id(),
                    opaque,
                )
                .map_err(|_| {
                    cx.throw_internal_error("failed to create Response instance");
                    -1
                })?;

            resp_obj.define_property_value_str(
                cx,
                "status",
                JSValue::from(meta.status as i32),
                zuqe::JS_PROP_C_W_E,
            )?;
            resp_obj.define_property_value_str(
                cx,
                "ok",
                JSValue::Bool(meta.status >= 200 && meta.status < 300),
                zuqe::JS_PROP_C_W_E,
            )?;
            {
                let s = cx.create_string_from_utf8(meta.status_text.into_bytes())?;
                resp_obj.define_property_value_str(
                    cx,
                    "statusText",
                    s.into(),
                    zuqe::JS_PROP_C_W_E,
                )?;
            }

            {
                let s = cx.create_string_from_utf8(meta.content_type.into_bytes())?;
                resp_obj.define_property_value_str(
                    cx,
                    "contentType",
                    s.into(),
                    zuqe::JS_PROP_C_W_E,
                )?;
            }

            resp_obj.define_property_value_str(
                cx,
                "contentLength",
                meta.content_length
                    .map(|v| JSValue::from(v as f64))
                    .unwrap_or(JSValue::NULL),
                zuqe::JS_PROP_C_W_E,
            )?;

            // headers (plain JS object)
            {
                let mut hdr_obj = cx.create_base_object()?;
                for (k, v) in &meta.headers {
                    let val = cx.create_string_from_utf8(v.as_bytes().to_vec())?;
                    hdr_obj.define_property_value_str(cx, k, val.into(), zuqe::JS_PROP_C_W_E)?;
                }
                resp_obj.define_property_value_str(
                    cx,
                    "headers",
                    hdr_obj.into(),
                    zuqe::JS_PROP_C_W_E,
                )?;
            }

            Ok(resp_obj.into()) // Promise resolve with the Response object
        },
    )?;

    //
    // spawn HTTP reqwest async task
    //
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut req = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
            &url_str,
        );
        for (k, v) in &req_headers {
            req = req.header(k, v);
        }
        if let Some(b) = &req_body {
            req = req.body(b.clone());
        }
        if let Some(dur) = timeout {
            req = req.timeout(dur);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let content_length = resp.content_length();

                let hdrs: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| v.to_str().ok().map(|vs| (k.to_string(), vs.to_string())))
                    .collect();

                let meta = ResponseMeta {
                    status: status.as_u16(),
                    status_text: status.canonical_reason().unwrap_or("").to_string(),
                    headers: hdrs,
                    content_type,
                    content_length,
                };

                if result_slot.clone().commit(Ok(meta)).is_err() {
                    return;
                }

                use futures_util::StreamExt;
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if body_tx.send(bytes.to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            body_tx
                                .send(format!("reqwest error: {e}").into_bytes())
                                .ok();
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                result_slot.commit(Err(format!("{e}"))).ok();
            }
        }
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Lazily create body AsyncIterableIterator from the stored receiver.
// On first access the receiver is taken; subsequent calls throw.
// ---------------------------------------------------------------------------

fn js_response_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = get_response_data(ctx, &cc.this())?;
    let rx = data.take_body_rx().ok_or_else(|| {
        ctx.throw_type_error("body already consumed");
        -1
    })?;

    let iter = AsyncIterableIterator::create_with_receiver::<Vec<u8>>(
        ctx,
        rx,
        |cx2, chunk| -> Result<Option<JSValue>, i32> {
            TypedArray::create_from_vec_u8(cx2, chunk)
                .map(JSValue::from)
                .map(Some)
        },
    )
    .map_err(|_| {
        ctx.throw_internal_error("failed to create body iterator");
        -1
    })?;

    Ok(iter.into())
}

const BODY_AS_BYTES: i32 = 0;
const BODY_AS_TEXT: i32 = 1;
const BODY_AS_JSON: i32 = 2;

// ---------------------------------------------------------------------------
// Response.bytes(), .text(), .json()  –  drain body, return Promise<T>
// Differ only in the conversion applied to the drained buffer.
// ---------------------------------------------------------------------------

fn js_response_body_as(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = get_response_data(ctx, &cc.this())?;
    let rx = data.take_body_rx().ok_or_else(|| {
        ctx.throw_type_error("body already consumed");
        -1
    })?;

    let magic = cc.magic();
    let result_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, payload| -> JSResult {
            let buf = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            match magic {
                BODY_AS_BYTES => TypedArray::create_from_vec_u8(cx, buf).map(JSValue::from),
                BODY_AS_TEXT => {
                    let s = String::from_utf8_lossy(&buf);
                    cx.create_string_from_utf8(s.as_bytes().to_vec())
                        .map(JSValue::from)
                }
                BODY_AS_JSON => {
                    let s = String::from_utf8_lossy(&buf);
                    let val: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
                        cx.throw_internal_error(&e.to_string());
                        -1
                    })?;
                    JSValue::from_serde_json(cx, &val)
                }
                _ => unreachable!(),
            }
        },
    )?;

    tokio::task::spawn_blocking(move || {
        let mut buf = Vec::new();
        while let Ok(chunk) = rx.recv() {
            buf.extend_from_slice(&chunk);
        }
        let _ = result_slot.commit(Ok(buf));
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Class registration
// ---------------------------------------------------------------------------

const RESPONSE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("body", Some(js_response_body), None, 0),
    FieldDef::FUNC("bytes", 0, js_response_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("text", 0, js_response_body_as, BODY_AS_TEXT as i16),
    FieldDef::FUNC("json", 0, js_response_body_as, BODY_AS_JSON as i16),
];

pub struct JSResponse;

impl JSResponse {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class_def(
            ClassDef::new("HttpResponse", RESPONSE_PROTO, None).on_dispose(
                |_, opaque: *mut c_void, _| {
                    if !opaque.is_null() {
                        drop(unsafe { Box::from_raw(opaque.cast::<ResponseData>()) });
                    }
                },
            ),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSResponse::register(rt)
}
