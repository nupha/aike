// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, TryRecvError};
use zuqe::{
    CallContext, Callback, Exception, JSResult, JSValue, JobState, ScopedContext,
    builtins::{
        array_buffer::ArrayBuffer,
        iterator::AsyncIterableIterator,
        json::JSON,
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
};

use crate::{
    html::{HtmlConvertMode, convert_html},
    http::client::headers::HeaderList,
    whatwg::{
        blob::{blob_bytes_of, blob_from_bytes, blob_type_of},
        form_data::{form_data_content_type, form_data_parse, form_data_to_bytes},
        stream::JSReadableStream,
        url::JSURLSearchParams,
    },
};

/// Source of response body data.
pub(crate) enum BodySource {
    /// Streaming from a real network response — poll the channel.
    /// Each item is `Ok(bytes)` for a chunk or `Err(msg)` when the underlying
    /// network stream fails; the latter must reject the body reader, not be
    /// treated as a normal chunk (see `JSReadableStream::from_channel`).
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
    // A FormData body serializes to multipart/form-data; without this it would
    // stringify to "[object FormData]".
    if let Some(bytes) = form_data_to_bytes(val) {
        return Ok(bytes);
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

    if let Some(t) = form_data_content_type(val) {
        return Some(t);
    }

    if JSURLSearchParams::payload_of(val).is_some() {
        return Some("application/x-www-form-urlencoded;charset=UTF-8".into());
    }

    // Fetch spec "extract a body": any value that is not a BufferSource
    // (ArrayBuffer / TypedArray / DataView) nor a ReadableStream is encoded as a
    // string via its `toString`, and defaults to this Content-Type. This covers
    // string literals and objects exposing `toString` (e.g.
    // `{ toString: () => "hi!" }`). BufferSources and streams carry no default
    // Content-Type, so they fall through to `None` below.
    let is_buffer_source = val.as_object_DataView().is_some()
        || val.as_object_ArrayBuffer().is_some()
        || val.as_object_TypedArray().is_some()
        || JSReadableStream::payload_of(val).is_some();
    (!is_buffer_source).then(|| "text/plain;charset=UTF-8".into())
}

/// Blob MIME type derived from a header list's current `Content-Type`: the
/// media type "essence" (type/subtype before any `;` parameter), trimmed and
/// lowercased. Absent `Content-Type` yields an empty string, per the Fetch
/// spec's "extract a MIME type" for blob bodies.
pub(crate) fn blob_type_from_header(list: &HeaderList) -> String {
    list.get("content-type")
        .map(|ct| {
            ct.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

pub(crate) const BODY_AS_BYTES: i32 = 0;
pub(crate) const BODY_AS_TEXT: i32 = 1;
pub(crate) const BODY_AS_JSON: i32 = 2;
/// Text mode with the optional html-convert arg ("strip" | "markdown").
/// Only wired on `HttpClientResponse.text()`; base `Response.text()` stays strict.
pub(crate) const BODY_AS_TEXT_CONVERT: i32 = 3;
/// Returns a `Blob` instance (type taken from `blob_type`).
pub(crate) const BODY_AS_BLOB: i32 = 4;
/// Returns a real `ArrayBuffer` (used by `Response.arrayBuffer()`; distinct
/// from `BODY_AS_BYTES`, which yields a `Uint8Array` view).
pub(crate) const BODY_AS_ARRAY_BUFFER: i32 = 5;
/// Returns a `FormData` parsed from the body. `blob_type` carries the full
/// `Content-Type` header (with parameters) so the multipart boundary and the
/// urlencoded variant can be told apart.
pub(crate) const BODY_AS_FORM_DATA: i32 = 6;

pub(crate) const BODY_TIMEOUT: Duration = Duration::from_secs(30);

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
                BODY_AS_ARRAY_BUFFER => ArrayBuffer::from_bytes(
                    cx,
                    JSValue::UNDEFINED,
                    buf.into_boxed_slice(),
                    std::ptr::null_mut(),
                )
                .map(JSValue::from),
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
                BODY_AS_FORM_DATA => {
                    // `blob_type` holds the raw Content-Type here, which
                    // selects the parse algorithm per the Fetch spec.
                    match form_data_parse(cx, &buf, &blob_type)? {
                        Some(v) => Ok(v),
                        None => Err(cx.throw_type_error("could not parse body as FormData")),
                    }
                }
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
            let mut s = stream;
            let reader = s.acquire_reader(ctx, false)?;
            reader.drain(ctx, result_slot, raw_err, BODY_TIMEOUT)?;
            Ok(promise.into())
        }
        BodySource::Channel(rx) => {
            let buf = Arc::new(Mutex::new(Some(Vec::new())));
            // Set once the body is committed (success, error, or timeout). The
            // commit happens from this polling job, which then must KEEP its
            // strong reference to `result_slot` alive until the result-slot
            // polling job (which holds only a `Weak`) has taken the data —
            // otherwise `weak.upgrade()` fails and panics on the very next
            // tick even though the data was committed.
            let committed = Arc::new(AtomicBool::new(false));
            let start = Instant::now();
            ctx.queue_job(
                move |_cx| {
                    if committed.load(Ordering::SeqCst) {
                        // Stay queued (holding the strong ref) until the
                        // result-slot job consumed the committed value.
                        return if result_slot.is_empty() {
                            Ok(JobState::Done)
                        } else {
                            Ok(JobState::Again)
                        };
                    }
                    let mut guard = buf.lock().unwrap();
                    let inner = guard.as_mut().unwrap();
                    let finish = |guard: &mut std::sync::MutexGuard<Option<Vec<u8>>>| {
                        guard.take();
                        committed.store(true, Ordering::SeqCst);
                    };
                    match rx.try_recv() {
                        Ok(Ok(chunk)) => {
                            inner.extend_from_slice(&chunk);
                            Ok(JobState::Again)
                        }
                        // A network error mid-body: reject the buffered read.
                        Ok(Err(msg)) => {
                            finish(&mut guard);
                            let _ = result_slot.commit(Err(msg));
                            Ok(JobState::Again)
                        }
                        Err(TryRecvError::Disconnected) => {
                            let full = guard.take().unwrap();
                            committed.store(true, Ordering::SeqCst);
                            let _ = result_slot.commit(Ok(full));
                            Ok(JobState::Again)
                        }
                        Err(TryRecvError::Empty) => {
                            if start.elapsed() > BODY_TIMEOUT {
                                finish(&mut guard);
                                let _ = result_slot.commit(Err("body read timeout".into()));
                                Ok(JobState::Again)
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
