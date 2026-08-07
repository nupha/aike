// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableStreamDefaultReader: the reader handle returned by
//! `ReadableStream.getReader()`.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, ErrorKind, Exception, FieldDef, JSResult, JSValue,
    JobGcTrace, JobState, Runtime, ScopedContext,
    builtins::{
        promise::{Promise, PromiseResultSlot, PromiseState},
        typed_array::TypedArray,
    },
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, whatwg::stream::readable_stream::JSReadableStream};

use super::{QueueEntry, RsState, make_iterator_result};

pub(crate) struct ReadableStreamDefaultReaderPayload {
    pub(super) stream: Option<JSReadableStream>,
    pub(crate) closed_promise: Option<Promise>,
    /// True when acquired via `getReader({mode: 'byob'})`. A BYOB read writes
    /// the chunk into the caller-supplied view instead of allocating one.
    pub(super) byob: bool,
}

impl ReadableStreamDefaultReaderPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref cp) = self.closed_promise {
            cp.trace(gcx);
        }
    }

    /// Perform a `read()` on the underlying stream and return a Promise that
    /// resolves to `{ value, done }`. Internal callers (drain/tee/pipe/async
    /// iteration) call this directly instead of going through the JS `read`
    /// property; the JS API entry point `js_reader_read` delegates here.
    pub(crate) fn read(
        &self,
        ctx: &mut ScopedContext,
        byob_view: Option<TypedArray>,
    ) -> Result<Promise, i32> {
        let mut stream_val = self
            .stream
            .ok_or_else(|| ctx.throw_type_error("reader released"))?;

        // Mark disturbed before any early return: even a read on a closed or
        // errored stream counts as consuming it.
        if let Some(sp) = stream_val.payload_mut() {
            sp.disturbed = true;
        }

        // Copy to decouple borrow: stream_p borrows sv, leaving stream_val free
        // for mutable access later.
        let sv = stream_val;
        let stream_p = sv
            .payload()
            .ok_or_else(|| ctx.throw_internal_error("invalid stream"))?;

        match stream_p.state {
            RsState::Errored => {
                return Promise::from_reject_value(ctx, stream_p.stored_error);
            }
            RsState::Closed => {
                let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
                return Promise::from_resolve_value(ctx, result);
            }
            RsState::Readable => {}
        }

        if let Some(controller_val) = stream_p.controller {
            let cp = controller_val
                .payload()
                .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

            if !cp.queue().borrow().is_empty() {
                let mut q = cp.queue().borrow_mut();
                let entry = q.remove(0);
                let total = cp.queue_total_size();
                cp.set_queue_total_size(total - entry.size as u32);

                // BYOB: copy as much of the chunk as fits into the caller's view
                // and push the remainder back so the next read continues where
                // this one stopped. Done before the close transition so a
                // partially consumed chunk keeps the stream readable.
                let chunk = if let Some(view) = byob_view {
                    let filled = byob_fill_view(ctx, view, entry.chunk, &mut q)?;
                    if !q.is_empty() {
                        cp.set_queue_total_size(cp.queue_total_size() + 1);
                    }
                    filled
                } else {
                    entry.chunk
                };

                // Per spec: after draining last item, if close_requested,
                // transition stream to closed state.
                if q.is_empty()
                    && cp.close_requested()
                    && let Some(sp) = stream_val.payload_mut()
                {
                    sp.state = RsState::Closed;
                }

                let result = make_iterator_result(ctx, chunk, false)?;
                return Promise::from_resolve_value(ctx, result);
            }

            if cp.close_requested() {
                let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
                return Promise::from_resolve_value(ctx, result);
            }

            // No data in queue — register a pending read on this controller so a
            // later enqueue()/close()/error() can resolve it. Stored per-controller
            // (not globally) so concurrent/sequential streams don't steal chunks.
            let (promise, rr) = Promise::create(ctx)?;
            cp.pending_read().borrow_mut().push([rr[0], rr[1]]);
            cp.call_pull(ctx)?;
            return Ok(promise.into());
        }

        // No controller (e.g. detached) — nothing to read.
        let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
        Promise::from_resolve_value(ctx, result)
    }

    /// Synchronously drain the stream's body into a byte buffer. Valid only
    /// for already-settled (queue-backed) reads; a pending read cannot be
    /// waited on synchronously and rejects with a TypeError.
    pub(crate) fn drain_sync(&self, ctx: &mut ScopedContext) -> Result<Vec<u8>, i32> {
        let mut buf = Vec::new();

        loop {
            let pr = self.read(ctx, None)?;
            match pr.state() {
                PromiseState::Fulfilled => {
                    let result = pr.result();
                    let done = result
                        .get_property_str(ctx, "done")
                        .and_then(|d| d.to_bool())
                        .unwrap_or(false);
                    if done {
                        break;
                    }
                    let value = result
                        .get_property_str(ctx, "value")
                        .map_err(|_| ctx.throw_internal_error("missing read value"))?;
                    let ta = TypedArray::try_from(value)
                        .map_err(|_| ctx.throw_type_error("body chunk is not a Uint8Array"))?;
                    ta.with_bytes(|b| buf.extend_from_slice(b));
                }

                // A stream that is not synchronously settled cannot be drained here.
                PromiseState::Pending | PromiseState::Rejected => {
                    return Err(ctx.throw_type_error("body stream is not synchronously readable"));
                }
            }
        }

        Ok(buf)
    }
}

const READER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("read", 0, js_reader_read, 0),
    FieldDef::FUNC("cancel", 1, js_reader_cancel, 0),
    FieldDef::FUNC("releaseLock", 0, js_reader_release_lock, 0),
    FieldDef::GETSET("closed", Some(js_reader_closed), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableStreamDefaultReader",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_reader_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error(
        "ReadableStreamDefaultReader must be obtained via ReadableStream.getReader()",
    ))
}

def_native_object!(
    "ReadableStreamDefaultReader",
    JSReadableStreamDefaultReader,
    ReadableStreamDefaultReaderPayload,
    READER_PROTO,
    Some(ClassCtorDef::new(js_reader_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableStreamDefaultReaderPayload::gc_trace
);

/// Copy `chunk` into the BYOB destination `view`, returning the view to hand
/// back to the caller. Bytes that do not fit are pushed to the front of `queue`
/// so the following read resumes from there.
///
/// The returned view spans exactly the written bytes, sharing the caller's
/// buffer, which is what makes `value.buffer` usable for the next read.
fn byob_fill_view(
    ctx: &mut ScopedContext,
    mut view: TypedArray,
    chunk: JSValue,
    queue: &mut Vec<QueueEntry>,
) -> Result<JSValue, i32> {
    let src: Vec<u8> = match chunk.as_object().and_then(|o| TypedArray::try_from(o).ok()) {
        Some(ta) => ta.with_bytes(|b| b.to_vec()),
        // Non-byte chunk in a byte stream: hand it through untouched.
        None => return Ok(chunk),
    };

    let n = view.with_bytes_mut(|dest| {
        let n = src.len().min(dest.len());
        dest[..n].copy_from_slice(&src[..n]);
        n
    });

    if n < src.len() {
        let rest = TypedArray::create_from_vec_u8(ctx, src[n..].to_vec())?;
        queue.insert(
            0,
            QueueEntry {
                chunk: JSValue::from(rest),
                size: 1.0,
            },
        );
    }

    // Return a view over the same buffer covering only the filled range, so the
    // caller can chain `value.buffer` plus `value.byteLength` as an offset.
    // `subarray` keeps the underlying buffer shared, unlike a fresh array.
    let view_val = JSValue::from(view);
    view_val.get_property_str(ctx, "subarray")?.call(
        ctx,
        view_val,
        &[JSValue::from(0.0), (n as f64).into()],
    )
}

fn js_reader_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;

    // A BYOB read takes the destination view as its only argument; the chunk is
    // written into that view's own buffer region rather than a fresh array.
    let byob_view = if p.byob {
        let view = cc.arg(0);
        let obj = view
            .as_object()
            .ok_or_else(|| ctx.throw_type_error("view must be a TypedArray"))?;
        let ta = TypedArray::try_from(obj)
            .map_err(|_| ctx.throw_type_error("view must be a TypedArray"))?;
        if ta.length() == 0 {
            return Err(ctx.throw_type_error("view must have non-zero length"));
        }
        Some(ta)
    } else {
        None
    };

    p.read(ctx, byob_view).map(JSValue::from)
}

fn js_reader_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let reason = cc.arg(0);
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;

    match p.stream {
        Some(mut rs) => rs
            .payload_mut()
            .unwrap()
            .cancel(ctx, reason)
            .map(JSValue::from),
        None => Err(ctx.throw_type_error("reader released")),
    }
}

fn js_reader_release_lock(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    if let Some(p) = JSReadableStreamDefaultReader::payload_mut_of(&mut this)
        && let Some(mut stream) = p.stream.take()
        && let Some(sp) = stream.payload_mut()
    {
        sp.reader = None;
    }
    Ok(JSValue::UNDEFINED)
}

fn js_reader_closed(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream reader"))?;
    if let Some(cp) = p.closed_promise {
        return Ok(JSValue::from(cp));
    }
    Ok(JSValue::UNDEFINED)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream_default_reader(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStreamDefaultReader::register(rt)
}

/// GC roots for the reader-drain job. The job outlives many event-loop turns,
/// so every JS value it can touch later must be traced from here, including the
/// cells it mutates (`pending` read promise, captured error value).
struct ReaderDrainRoots {
    reader: JSValue,
    pending: Rc<RefCell<Option<Promise>>>,
    raw_err: Rc<RefCell<Option<JSValue>>>,
}

impl JobGcTrace for ReaderDrainRoots {
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

impl JSReadableStreamDefaultReader {
    /// Pump this reader to completion, committing the concatenated bytes (or
    /// the stream's error) into `slot`.
    ///
    /// Runs as a polling job because `read()` returns a promise that may settle
    /// later (e.g. from an async `pull()`); we inspect its state each tick
    /// rather than attaching JS callbacks, which native fn-pointer callbacks
    /// cannot capture state for.
    pub(crate) fn drain(
        &self,
        ctx: &mut ScopedContext,
        slot: PromiseResultSlot<Result<Vec<u8>, String>>,
        raw_err: Rc<RefCell<Option<JSValue>>>,
        timeout: Duration,
    ) -> Result<(), i32> {
        let reader = *self;

        // Holds the in-flight read() promise between polls.
        let pending = Rc::new(RefCell::new(None::<Promise>));
        let start = Instant::now();
        let roots = ReaderDrainRoots {
            reader: JSValue::from(reader),
            pending: pending.clone(),
            raw_err: raw_err.clone(),
        };

        // Accumulated body bytes across polling turns. The job closure is `Fn`
        // (not `FnMut`), so interior mutability via RefCell is required to grow
        // `buf` on each turn.
        let buf = Rc::new(RefCell::new(Vec::<u8>::new()));

        ctx.queue_job(
            move |cx| {
                // Start a new read() unless one is already in flight.
                if pending.borrow().is_none() {
                    let p = reader
                        .payload()
                        .ok_or_else(|| cx.throw_type_error("reader released"))?;

                    match p.read(cx, None) {
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
                        if start.elapsed() > timeout {
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
                        // Uint8Array chunks (see "extract a byte stream"); any
                        // other chunk type must reject the consume promise with
                        // a TypeError.
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
}
