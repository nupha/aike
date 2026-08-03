// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, TryRecvError};
use zuqe::{
    CallContext, Callback, ErrorKind, Exception, JSResult, JSValue, JobGcTrace, JobState,
    ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        json::JSON,
        promise::{Promise, PromiseResultSlot, PromiseState},
        typed_array::TypedArray,
    },
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    html::{HtmlConvertMode, convert_html},
    whatwg::{
        blob::{blob_bytes_of, blob_from_bytes, blob_type_of},
        stream::{JSReadableStream, call_opt_method},
        url::JSURLSearchParams,
    },
};

/// Source of response body data.
pub(crate) enum BodySource {
    /// Streaming from a real network response — poll the channel.
    /// Each item is `Ok(bytes)` for a chunk or `Err(msg)` when the underlying
    /// network stream fails; the latter must reject the body reader, not be
    /// treated as a normal chunk (see `readable_stream_from_channel`).
    Channel(Receiver<Result<Vec<u8>, String>>),
    /// Entire body is already available as bytes (synthetic response).
    Synthetic(Vec<u8>),
    /// A JS `ReadableStream` body — drain by pumping its reader.
    Stream(JSReadableStream),
}

/// Convert a JS body argument (string or Uint8Array/TypedArray) into bytes.
/// A `ReadableStream` body is NOT handled here — it must be kept as a stream by
/// the caller, otherwise `to_string` would yield "[object ReadableStream]".
pub(crate) fn body_to_bytes(ctx: &mut ScopedContext, val: &JSValue) -> Result<Vec<u8>, i32> {
    if let Some(bytes) = blob_bytes_of(val) {
        return Ok(bytes.to_vec());
    }
    if let Ok(ta) = TypedArray::try_from(val.clone()) {
        return Ok(ta.with_bytes(|b| b.to_vec()));
    }
    // DataView and ArrayBuffer are distinct classes from TypedArray — handle
    // them explicitly so they are serialized as their underlying bytes rather
    // than "[object DataView]" / "[object ArrayBuffer]".
    if let Some(dv) = val.as_object_DataView() {
        return Ok(dv.payload().with_bytes(|b| b.to_vec()));
    }
    if let Some(ab) = val.as_object_ArrayBuffer() {
        return Ok(ab.payload().with_bytes(|b| b.to_vec()));
    }
    let s = val.to_string(ctx)?;
    Ok(s.content().encode_utf8(false).as_bytes().to_vec())
}

/// WHATWG fetch "extract a body": the body's source determines a default
/// `Content-Type`. Buffer sources (ArrayBuffer / TypedArray / DataView) have
/// none, and a `Blob` contributes its own type only when non-empty.
///
/// Returns `None` when the source implies no default header.
pub(crate) fn body_content_type(val: &JSValue) -> Option<String> {
    if let Some(t) = blob_type_of(val) {
        return (!t.is_empty()).then_some(t);
    }

    if JSURLSearchParams::payload_of(val).is_some() {
        return Some("application/x-www-form-urlencoded;charset=UTF-8".into());
    }

    // Strings are the only remaining source with a default type. Checked last
    // because Blob/URLSearchParams are objects, not strings.
    val.is_string().then(|| "text/plain;charset=UTF-8".into())
}

pub(crate) const BODY_AS_BYTES: i32 = 0;
pub(crate) const BODY_AS_TEXT: i32 = 1;
pub(crate) const BODY_AS_JSON: i32 = 2;
/// Text mode with the optional html-convert arg ("strip" | "markdown").
/// Only wired on `HttpClientResponse.text()`; base `Response.text()` stays strict.
pub(crate) const BODY_AS_TEXT_CONVERT: i32 = 3;
/// Returns a `Blob` instance (type taken from `blob_type`).
pub(crate) const BODY_AS_BLOB: i32 = 4;

const BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Drain a body source into a `Promise` resolving to text/json/bytes.
/// `html_convert` applies only in text mode for `text/html` responses.
pub(crate) fn drain_body(
    ctx: &mut ScopedContext,
    source: BodySource,
    mode: i32,
    html_convert: Option<HtmlConvertMode>,
    blob_type: String,
) -> JSResult {
    let result_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
    // Carries a JS rejection value alongside the (Send-only) slot, so a stream
    // error can be rethrown as the original object rather than a copied string.
    let raw_err: Rc<RefCell<Option<JSValue>>> = Rc::new(RefCell::new(None));
    let convert_err = raw_err.clone();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), move |cx, data| -> JSResult {
            let buf = data.map_err(|e| match convert_err.borrow_mut().take() {
                Some(v) => {
                    cx.throw(Exception::Error(v));
                    -1
                }
                None => cx.throw_internal_error(&e),
            })?;
            match mode {
                BODY_AS_BYTES => TypedArray::create_from_vec_u8(cx, buf).map(JSValue::from),
                BODY_AS_TEXT => {
                    // WHATWG "UTF-8 decode" strips a single leading U+FEFF BOM
                    // (see decode branch of BODY_AS_JSON above). text() must
                    // match that, otherwise consumers see a stray BOM char.
                    let bytes = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&buf);
                    let s = String::from_utf8_lossy(bytes);
                    let out = match html_convert {
                        Some(m) => convert_html(&s, m),
                        None => s.into_owned(),
                    };
                    cx.create_string_from_utf8(out.into_bytes())
                        .map(JSValue::make_string)
                }
                BODY_AS_JSON => {
                    // WHATWG: JSON parse = UTF-8 decode the bytes, then run the
                    // JS JSON parse algorithm. That throws a `SyntaxError` on
                    // invalid input (e.g. UTF-16) and uses the engine's own
                    // duplicate-key / key-order semantics — so we must call the
                    // JS `JSON.parse`, not serde_json. "UTF-8 decode" also
                    // strips a leading BOM, which JSON.parse itself rejects.
                    let bytes = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&buf);
                    let text = String::from_utf8_lossy(bytes);
                    JSON::parse(cx, &text)
                }
                BODY_AS_BLOB => blob_from_bytes(cx, buf.into(), blob_type),
                _ => unreachable!(),
            }
        })?;

    match source {
        BodySource::Synthetic(buf) => {
            // Entire body is already available — commit directly, no polling needed.
            let _ = result_slot.commit(Ok(buf));
            Ok(promise.into())
        }
        BodySource::Stream(stream) => {
            drain_readable_stream(ctx, stream, result_slot, raw_err)?;
            Ok(promise.into())
        }
        BodySource::Channel(rx) => {
            let buf = Arc::new(Mutex::new(Some(Vec::new())));
            let start = Instant::now();
            ctx.queue_job(
                move |_cx| {
                    let mut guard = buf.lock().unwrap();
                    let inner = guard.as_mut().unwrap();
                    match rx.try_recv() {
                        Ok(Ok(chunk)) => {
                            inner.extend_from_slice(&chunk);
                            Ok(JobState::Again)
                        }
                        // A network error mid-body: reject the buffered read.
                        Ok(Err(msg)) => {
                            guard.take();
                            let _ = result_slot.commit(Err(msg));
                            Ok(JobState::Done)
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
    }
}

/// GC roots for the stream-drain job. The job outlives many event-loop turns,
/// so every JS value it can touch later must be traced from here, including the
/// cells it mutates (`pending` read promise, captured error value).
struct DrainRoots {
    reader: JSValue,
    pending: Rc<RefCell<Option<Promise>>>,
    raw_err: Rc<RefCell<Option<JSValue>>>,
}

impl JobGcTrace for DrainRoots {
    fn job_trace_values(&self, gcx: &mut GcTraceCtx) {
        self.reader.trace(gcx);
        if let Some(p) = *self.pending.borrow() {
            JSValue::from(p).trace(gcx);
        }
        if let Some(ref v) = *self.raw_err.borrow() {
            v.trace(gcx);
        }
    }
}

/// Pump a JS `ReadableStream` to completion, committing the concatenated bytes
/// (or the stream's error) into `slot`.
///
/// Runs as a polling job because `reader.read()` returns a promise that may
/// settle later (e.g. from an async `pull()`); we inspect its state each tick
/// rather than attaching JS callbacks, which native fn-pointer callbacks
/// cannot capture state for.
fn drain_readable_stream(
    ctx: &mut ScopedContext,
    stream: JSReadableStream,
    slot: PromiseResultSlot<Result<Vec<u8>, String>>,
    raw_err: Rc<RefCell<Option<JSValue>>>,
) -> Result<(), i32> {
    let reader = match call_opt_method(ctx, stream.into(), "getReader", &[])? {
        Some(r) if r.is_object() => r,
        // Already locked by someone else: surface as a body error.
        _ => {
            let _ = slot.commit(Err("body stream is already locked".into()));
            return Ok(());
        }
    };

    let buf = Rc::new(RefCell::new(Vec::<u8>::new()));
    // Holds the in-flight read() promise between polls.
    let pending = Rc::new(RefCell::new(None::<Promise>));
    let start = Instant::now();
    let roots = DrainRoots {
        reader,
        pending: pending.clone(),
        raw_err: raw_err.clone(),
    };

    ctx.queue_job(
        move |cx| {
            // Start a new read() unless one is already in flight.
            if pending.borrow().is_none() {
                let read_fn = reader.get_property_str(cx, "read")?;
                if !read_fn.is_callable() {
                    let _ = slot.commit(Err("stream reader has no read()".into()));
                    return Ok(JobState::Done);
                }
                match read_fn.call(cx, reader, &[]) {
                    Ok(pv) => match Promise::try_from(pv) {
                        Ok(p) => *pending.borrow_mut() = Some(p),
                        Err(_) => {
                            let _ = slot.commit(Err("read() did not return a promise".into()));
                            return Ok(JobState::Done);
                        }
                    },
                    Err(_) => {
                        // read() threw synchronously — carry the thrown value
                        // over so identity is preserved.
                        if let Exception::Error(v) = cx.take_exception() {
                            *raw_err.borrow_mut() = Some(v);
                        }
                        let _ = slot.commit(Err("stream read failed".into()));
                        return Ok(JobState::Done);
                    }
                }
            }

            let p = pending.borrow().unwrap();
            match p.state() {
                PromiseState::Pending => {
                    if start.elapsed() > BODY_TIMEOUT {
                        let _ = slot.commit(Err("body read timeout".into()));
                        return Ok(JobState::Done);
                    }
                    Ok(JobState::Again)
                }
                PromiseState::Rejected => {
                    // Stream errored — hand over the original reason value (not
                    // a stringified copy) so identity is preserved, as
                    // `promise_rejects_exactly` and user `catch` blocks expect.
                    *raw_err.borrow_mut() = Some(p.result());
                    let _ = slot.commit(Err("stream errored".into()));
                    Ok(JobState::Done)
                }
                PromiseState::Fulfilled => {
                    *pending.borrow_mut() = None;
                    let result = p.result();
                    let done = result
                        .get_property_str(cx, "done")
                        .and_then(|d| d.to_bool())
                        .unwrap_or(false);
                    if done {
                        let full = std::mem::take(&mut *buf.borrow_mut());
                        let _ = slot.commit(Ok(full));
                        return Ok(JobState::Done);
                    }
                    let value = match result.get_property_str(cx, "value") {
                        Ok(v) => v,
                        // Missing `value` on a non-done chunk — malformed.
                        _ => {
                            *raw_err.borrow_mut() = cx
                                .create_error_object(
                                    ErrorKind::Type,
                                    "body chunk is not a Uint8Array",
                                    false,
                                )
                                .map(Into::into);
                            let _ = slot.commit(Err("body chunk is not a Uint8Array".into()));
                            return Ok(JobState::Done);
                        }
                    };

                    // Per WHATWG fetch, a ReadableStream body must yield
                    // Uint8Array chunks (see "extract a byte stream"); any other
                    // chunk type must reject the consume promise with a TypeError.
                    let Ok(ta) = TypedArray::try_from(value) else {
                        *raw_err.borrow_mut() = cx
                            .create_error_object(
                                ErrorKind::Type,
                                "body chunk is not a Uint8Array",
                                false,
                            )
                            .map(Into::into);
                        let _ = slot.commit(Err("body chunk is not a Uint8Array".into()));
                        return Ok(JobState::Done);
                    };
                    ta.with_bytes(|b| buf.borrow_mut().extend_from_slice(b));

                    Ok(JobState::Again)
                }
            }
        },
        roots,
    );
    Ok(())
}

/// Create an async-iterable body stream from a `Receiver`, attaching a
/// `cancel()` method that releases the connection (see module docs).
pub(crate) fn make_body_async_iterator(ctx: &mut ScopedContext, rx: Receiver<Vec<u8>>) -> JSResult {
    let iter = AsyncIterableIterator::create_with_receiver::<Vec<u8>>(
        ctx,
        rx,
        |cx2, chunk| -> Result<Option<JSValue>, i32> {
            TypedArray::create_from_vec_u8(cx2, chunk)
                .map(JSValue::from)
                .map(Some)
        },
    )
    .map_err(|_| ctx.throw_internal_error("failed to create body stream"))?;

    let mut iter_val: JSValue = iter.into();
    let cancel_fn = ctx.create_native_function(Callback::Func(js_stream_cancel), "cancel", 1, 0)?;
    iter_val.define_property_value_str(ctx, "cancel", cancel_fn.into(), zuqe::JS_PROP_C_W_E)?;
    Ok(iter_val)
}

/// `body.cancel()` — delegates to the async iterator's `return()`, which
/// drops the underlying `Receiver` and stops the producer task.
fn js_stream_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let ret = this.get_property_str(ctx, "return")?;
    let cb = ret.ensure_callable(ctx)?;
    cb.call(ctx, this, &[])?;
    Ok(JSValue::UNDEFINED)
}
