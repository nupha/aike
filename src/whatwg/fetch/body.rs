// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Shared body consumption helpers for WHATWG Fetch `Body` mixin.
//!
//! Both the client `Response` and `Request` reuse these: draining a body
//! channel into text/json/bytes, and creating an async-iterable stream.
//! Cancel semantics: dropping the body `Receiver` (via `return()` on the
//! async iterator, or `response.close()`) stops the producer and releases
//! the underlying connection — no extra state machine needed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, TryRecvError};
use zuqe::{
    CallContext, Callback, JobState, JSResult, JSValue, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
};

use crate::html::{HtmlConvertMode, convert_html};

/// Convert a JS body argument (string or Uint8Array/TypedArray) into bytes.
pub(crate) fn body_to_bytes(ctx: &mut ScopedContext, val: &JSValue) -> Result<Vec<u8>, i32> {
    if let Ok(mut ta) = TypedArray::try_from(val.clone()) {
        return Ok(ta.with_bytes_mut(|b| b.to_vec()));
    }
    let s = val.to_string(ctx)?;
    Ok(s.content().encode_utf8(false).as_bytes().to_vec())
}

pub(crate) const BODY_AS_BYTES: i32 = 0;
pub(crate) const BODY_AS_TEXT: i32 = 1;
pub(crate) const BODY_AS_JSON: i32 = 2;

const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Drain a body `Receiver` into a `Promise` resolving to text/json/bytes.
/// `html_convert` applies only in text mode for `text/html` responses.
pub(crate) fn drain_body(
    ctx: &mut ScopedContext,
    rx: Receiver<Vec<u8>>,
    mode: i32,
    html_convert: Option<HtmlConvertMode>,
) -> JSResult {
    let result_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, data| -> JSResult {
            let buf = data.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            match mode {
                BODY_AS_BYTES => TypedArray::create_from_vec_u8(cx, buf).map(JSValue::from),
                BODY_AS_TEXT => {
                    let s = String::from_utf8_lossy(&buf);
                    let out = match html_convert {
                        Some(m) => convert_html(&s, m),
                        None => s.into_owned(),
                    };
                    cx.create_string_from_utf8(out.into_bytes()).map(JSValue::from)
                }
                BODY_AS_JSON => {
                    let val: serde_json::Value = serde_json::from_slice(&buf).map_err(|e| {
                        cx.throw_internal_error(&e.to_string());
                        -1
                    })?;
                    JSValue::from_serde_json(cx, &val)
                }
                _ => unreachable!(),
            }
        },
    )?;

    let buf = Arc::new(Mutex::new(Some(Vec::new())));
    let start = Instant::now();
    ctx.queue_job(
        move |_cx| {
            let mut guard = buf.lock().unwrap();
            let inner = guard.as_mut().unwrap();
            match rx.try_recv() {
                Ok(chunk) => {
                    inner.extend_from_slice(&chunk);
                    Ok(JobState::Again)
                }
                Err(TryRecvError::Disconnected) => {
                    let full = guard.take().unwrap();
                    let _ = result_slot.commit(Ok(full));
                    Ok(JobState::Done)
                }
                Err(TryRecvError::Empty) => {
                    if start.elapsed() > BODY_TIMEOUT {
                        guard.take();
                        let _ = result_slot.commit(Err("body read timeout".into()));
                        Ok(JobState::Done)
                    } else {
                        Ok(JobState::Again)
                    }
                }
            }
        },
        (),
    );

    Ok(promise.into())
}

/// Create an async-iterable body stream from a `Receiver`, attaching a
/// `cancel()` method that releases the connection (see module docs).
pub(crate) fn make_body_stream(ctx: &mut ScopedContext, rx: Receiver<Vec<u8>>) -> JSResult {
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
        ctx.throw_internal_error("failed to create body stream");
        -1
    })?;

    let mut iter_val: JSValue = iter.into();
    let cancel_fn =
        ctx.create_native_function(Callback::Func(js_stream_cancel), "cancel", 1, 0)?;
    iter_val.define_property_value_str(ctx, "cancel", cancel_fn.into(), zuqe::JS_PROP_C_W_E)?;
    Ok(iter_val)
}

/// `body.cancel()` — delegates to the async iterator's `return()`, which
/// drops the underlying `Receiver` and stops the producer task.
fn js_stream_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let ret = this.get_property_str(ctx, "return")?;
    let cb = ret.into_callable(ctx)?;
    cb.call(ctx, this, &[])?;
    Ok(JSValue::UNDEFINED)
}
