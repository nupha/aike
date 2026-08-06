// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableStream, its default reader/controller, async iterator support and
//! channel/bytes-backed stream constructors.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, TryRecvError};
use linkme::distributed_slice;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, ErrorKind, FieldDef, JSResult, JSValue, JobState,
    Runtime, ScopedContext,
    builtins::{
        iterator::{AsyncIterableIterator, AsyncIteratorFetcher, IterFetchStatus},
        promise::Promise,
        typed_array::TypedArray,
    },
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::{
    QueueEntry, RsState, call_opt_method, make_iterator_result, make_rejected_promise,
    make_resolved_promise, settle_stream_state,
};
use crate::whatwg::abort::{signal_is_aborted, signal_reason};
use crate::whatwg::dom::make_dom_exception;

pub(crate) struct ReadableStreamPayload {
    pub(crate) state: RsState,
    pub(crate) stored_error: JSValue,
    pub(crate) reader: Option<JSReadableStreamDefaultReader>,
    pub(crate) controller: Option<JSReadableStreamDefaultController>,
    /// Set once read() is called. Unlike the reader lock this is never
    /// cleared by releaseLock(), so a used stream stays unusable as a body.
    pub(crate) disturbed: bool,
    /// Abort signal tied to this body's fetch. When aborted mid-body the stream
    /// errors with the signal's reason. Stored on the stream payload (not just
    /// the channel registry) so GC keeps it alive while the stream is live.
    pub(crate) signal: Option<JSValue>,
    /// Id of this stream's entry in CHANNEL_STREAMS, so the entry can be
    /// removed when the stream is garbage-collected. The registry is a plain
    /// RefCell that GC does not scan, so a collected stream would otherwise
    /// leave a dangling controller handle for the poller to trip over.
    pub(crate) channel_id: Option<u64>,
}

impl ReadableStreamPayload {
    pub(crate) fn new() -> Self {
        Self {
            state: RsState::Readable,
            stored_error: JSValue::UNDEFINED,
            reader: None,
            controller: None,
            disturbed: false,
            signal: None,
            channel_id: None,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.stored_error.trace(gcx);
        if let Some(ref r) = self.reader {
            r.trace(gcx);
        }
        if let Some(ref c) = self.controller {
            c.trace(gcx);
        }
        if let Some(ref s) = self.signal {
            s.trace(gcx);
        }
        // Drop the poller registry entry so a collected stream is never visited
        // via a dangling controller handle. Single-threaded, GC phase: safe.
        if let Some(id) = self.channel_id {
            CHANNEL_STREAMS.with(|m| {
                m.borrow_mut().remove(&id);
            });
        }
    }
}

const READABLE_STREAM_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("locked", Some(js_readable_stream_locked), None, 0),
    FieldDef::GETSET("state", Some(js_readable_stream_state), None, 0),
    FieldDef::FUNC("getReader", 0, js_readable_stream_get_reader, 0),
    FieldDef::FUNC("cancel", 1, js_readable_stream_cancel, 0),
    FieldDef::FUNC("pipeTo", 1, js_readable_stream_pipe_to, 0),
    FieldDef::FUNC("pipeThrough", 1, js_readable_stream_pipe_through, 0),
    FieldDef::FUNC(
        "[Symbol.asyncIterator]",
        0,
        js_readable_stream_async_iterator,
        0,
    ),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableStream",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "ReadableStream",
    JSReadableStream,
    ReadableStreamPayload,
    READABLE_STREAM_PROTO,
    Some(ClassCtorDef::new(js_readable_stream_constructor, 1, false)
        .expose_to_global()),
    on_payload_gc: ReadableStreamPayload::gc_trace
);

fn js_readable_stream_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let underlying_source = cc.arg(0);

    let payload = ReadableStreamPayload::new();
    let mut instance = JSReadableStream::new_instance(ctx, new_target, payload)?;

    let controller = create_readable_stream_controller(ctx, &instance, &underlying_source)?;
    instance.payload_mut().unwrap().controller = Some(controller);

    let _ = call_opt_method(
        ctx,
        underlying_source,
        "start",
        &[JSValue::from(controller)],
    );

    Ok(instance.into())
}

fn js_readable_stream_locked(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;
    Ok(JSValue::from(p.reader.is_some()))
}

fn js_readable_stream_state(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;
    let s = match p.state {
        RsState::Readable => "readable",
        RsState::Closed => "closed",
        RsState::Errored => "errored",
    };
    let sref = ctx
        .create_string_from_utf8(s.as_bytes().to_vec())
        .map_err(|_| -1)?;
    Ok(JSValue::make_string(sref))
}

fn js_readable_stream_get_reader(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    let p = JSReadableStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;
    if p.reader.is_some() {
        return Err(ctx.throw_type_error("ReadableStream is already locked"));
    }

    // options.mode: undefined -> default reader, "byob" -> BYOB reader,
    // anything else -> TypeError (the spec converts mode to a fixed enum).
    let options = cc.arg(0);
    let byob = if options.is_undefined() || options.is_null() {
        false
    } else {
        let mode = options.get_property_str(ctx, "mode")?;
        if mode.is_undefined() {
            false
        } else if mode.to_string(ctx)?.content().encode_utf8(false).as_ref() == "byob" {
            true
        } else {
            return Err(ctx.throw_type_error("`mode` must be undefined or \"byob\""));
        }
    };

    let mut closed_promise = Promise::create(ctx)?.0;
    if let Some(sp) = JSReadableStream::payload_of(&this) {
        match sp.state {
            RsState::Closed => closed_promise.resolve(ctx, JSValue::UNDEFINED),
            RsState::Errored => closed_promise.reject(ctx, sp.stored_error),
            RsState::Readable => {}
        }
    }

    let reader = JSReadableStreamDefaultReader::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultReaderPayload {
            stream: Some(JSReadableStream::try_from(this)?),
            closed_promise: Some(closed_promise),
            byob,
        },
    )?;
    JSReadableStream::payload_mut_of(&mut this).unwrap().reader = Some(reader);

    Ok(JSValue::from(reader))
}

fn js_readable_stream_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    readable_stream_cancel(ctx, &cc.this(), cc.arg(0))
}

// Pipe machinery: pipeTo locks the source synchronously (disturbing it per
// WHATWG spec) and copies data to `dest` asynchronously. The WPT tests only
// require that locking/disturbing happens synchronously; the transfer runs as
// a microtask chain via reader.read()/writer.write() promises.
//
// Callback `data` layout shared across the async driver:
//   [0] = result resolve fn, [1] = result reject fn, [2] = reader, [3] = writer

fn js_readable_stream_pipe_to(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let dest = cc.arg(0);
    let _options = cc.arg(1);
    let writer = dest
        .get_property_str(ctx, "getWriter")?
        .call(ctx, dest, &[])?;
    // Acquire a reader on the source synchronously: this locks (disturbs) the
    // stream immediately so `locked` is true and a later getReader() throws.
    let reader = this
        .get_property_str(ctx, "getReader")?
        .call(ctx, this, &[])?;
    let (promise, rr) = Promise::create(ctx)?;
    let data = [rr[0], rr[1], reader, writer];
    let step = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_read),
            "pipeToOnRead",
            1,
            0,
            data.into(),
        )?
        .into();
    let step_fail = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_read_error),
            "pipeToOnReadErr",
            1,
            0,
            data.into(),
        )?
        .into();

    let mut read_promise: Promise = Promise::try_from(
        reader
            .get_property_str(ctx, "read")?
            .call(ctx, reader, &[])?,
    )?;
    read_promise.then(ctx, step, step_fail)?;

    Ok(JSValue::from(promise))
}

fn js_readable_stream_pipe_through(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let transform = cc.arg(0);
    let _options = cc.arg(1);

    // pipeThrough = source.pipeTo(transform.writable) then return transform.readable.
    let writable = transform.get_property_str(ctx, "writable")?;
    let readable = transform.get_property_str(ctx, "readable")?;
    this.get_property_str(ctx, "pipeTo")?
        .call(ctx, this, &[writable])?;

    Ok(readable)
}

// Read one chunk and schedule the next step.
fn pipe_to_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reader = cc.data(2);
    let writer = cc.data(3);
    let read_js = reader
        .get_property_str(ctx, "read")?
        .call(ctx, reader, &[])?;
    let mut read_promise: Promise = Promise::try_from(read_js)?;
    let step = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_read),
            "pipeToOnRead",
            1,
            0,
            [cc.data(0), cc.data(1), reader, writer].into(),
        )?
        .into();
    let step_fail = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_read_error),
            "pipeToOnReadErr",
            1,
            0,
            [cc.data(0), cc.data(1), reader, writer].into(),
        )?
        .into();
    let _ = read_promise.then(ctx, step, step_fail);
    Ok(JSValue::UNDEFINED)
}

// A chunk was read: close on done, otherwise write and loop.
fn pipe_to_on_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let result = cc.arg(0);
    let done = result.get_property_str(ctx, "done")?.to_bool()?;
    if done {
        let resolve = cc.data(0);
        let writer = cc.data(3);
        // Close the destination; resolve the pipe promise when done.
        let close_js = writer
            .get_property_str(ctx, "close")?
            .call(ctx, writer, &[])?;
        let mut close_promise: Promise = Promise::try_from(close_js)?;
        let settle = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_to_settle),
                "pipeToSettle",
                0,
                0,
                [
                    resolve,
                    JSValue::UNDEFINED,
                    JSValue::UNDEFINED,
                    JSValue::UNDEFINED,
                ]
                .into(),
            )?
            .into();
        let _ = close_promise.then(ctx, settle, settle);
        return Ok(JSValue::UNDEFINED);
    }
    let value = result.get_property_str(ctx, "value")?;
    let writer = cc.data(3);
    let write_js = writer
        .get_property_str(ctx, "write")?
        .call(ctx, writer, &[value])?;
    let mut write_promise: Promise = Promise::try_from(write_js)?;
    let loop_fn = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_read),
            "pipeToRead",
            1,
            0,
            [cc.data(0), cc.data(1), cc.data(2), writer].into(),
        )?
        .into();
    let write_fail = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_write_error),
            "pipeToWriteErr",
            1,
            0,
            [cc.data(0), cc.data(1), cc.data(2), writer].into(),
        )?
        .into();
    let _ = write_promise.then(ctx, loop_fn, write_fail);
    Ok(JSValue::UNDEFINED)
}

// Source read failed: abort the writer and reject the pipe promise.
fn pipe_to_on_read_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    let reject = cc.data(1);
    let writer = cc.data(3);
    let reader = cc.data(2);
    writer
        .get_property_str(ctx, "abort")?
        .call(ctx, writer, &[reason])?;
    reader
        .get_property_str(ctx, "cancel")?
        .call(ctx, reader, &[reason])?;
    reject.call(ctx, JSValue::UNDEFINED, &[reason])?;
    Ok(JSValue::UNDEFINED)
}

// Write failed: abort the writer, cancel the reader and reject the pipe promise.
fn pipe_to_on_write_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    let reject = cc.data(1);
    let writer = cc.data(3);
    let reader = cc.data(2);
    writer
        .get_property_str(ctx, "abort")?
        .call(ctx, writer, &[reason])?;
    reader
        .get_property_str(ctx, "cancel")?
        .call(ctx, reader, &[reason])?;
    reject.call(ctx, JSValue::UNDEFINED, &[reason])?;
    Ok(JSValue::UNDEFINED)
}

// Resolve the pipe promise once the destination closes (close success or fail).
fn pipe_to_settle(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let resolve = cc.data(0);
    resolve.call(ctx, JSValue::UNDEFINED, &[])?;
    Ok(JSValue::UNDEFINED)
}

fn js_readable_stream_async_iterator(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    create_stream_async_iterator(ctx, &cc.this())
}

pub(crate) struct ReadableStreamDefaultReaderPayload {
    stream: Option<JSReadableStream>,
    pub(crate) closed_promise: Option<Promise>,
    /// True when acquired via `getReader({mode: 'byob'})`. A BYOB read writes
    /// the chunk into the caller-supplied view instead of allocating one.
    byob: bool,
}

thread_local! {
    /// Channel-backed streams being polled: id -> (controller, receiver, idle deadline).
    /// The deadline is per stream and refreshed on every chunk, so a slow but
    /// live producer is never cut off; only a stalled stream times out.
    pub(super) static CHANNEL_STREAMS: RefCell<
        HashMap<u64, (JSReadableStreamDefaultController, Receiver<Result<Vec<u8>, String>>, Instant)>,
    > = RefCell::new(HashMap::new());

    static NEXT_STREAM_ID: Cell<u64> = const { Cell::new(1) };
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

    let mut stream_val = p
        .stream
        .ok_or_else(|| ctx.throw_type_error("reader released"))?;

    // Mark disturbed before any early return: even a read on a closed or
    // errored stream counts as consuming it.
    if let Some(sp) = stream_val.payload_mut() {
        sp.disturbed = true;
    }

    // Copy to decouple borrow: stream_p borrows sv, leaving stream_val free for mutable access later.
    let sv = stream_val;
    let stream_p = sv
        .payload()
        .ok_or_else(|| ctx.throw_internal_error("invalid stream"))?;

    match stream_p.state {
        RsState::Errored => {
            return make_rejected_promise(ctx, stream_p.stored_error);
        }
        RsState::Closed => {
            let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
            return make_resolved_promise(ctx, result);
        }
        RsState::Readable => {}
    }

    if let Some(controller_val) = stream_p.controller {
        let cp = controller_val
            .payload()
            .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

        if !cp.queue.borrow().is_empty() {
            let mut q = cp.queue.borrow_mut();
            let entry = q.remove(0);
            let total = cp.queue_total_size.get();
            cp.queue_total_size.set(total - entry.size);

            // BYOB: copy as much of the chunk as fits into the caller's view and
            // push the remainder back so the next read continues where this one
            // stopped. Done before the close transition so a partially consumed
            // chunk keeps the stream readable.
            let chunk = if let Some(view) = byob_view {
                let filled = byob_fill_view(ctx, view, entry.chunk, &mut q)?;
                if !q.is_empty() {
                    cp.queue_total_size.set(cp.queue_total_size.get() + 1.0);
                }
                filled
            } else {
                entry.chunk
            };

            // Per spec: after draining last item, if close_requested,
            // transition stream to closed state.
            if q.is_empty()
                && cp.close_requested.get()
                && let Some(sp) = stream_val.payload_mut()
            {
                sp.state = RsState::Closed;
            }

            let result = make_iterator_result(ctx, chunk, false)?;
            return make_resolved_promise(ctx, result);
        }

        if cp.close_requested.get() {
            let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
            return make_resolved_promise(ctx, result);
        }

        // No data in queue — register a pending read on this controller so a
        // later enqueue()/close()/error() can resolve it. Stored per-controller
        // (not globally) so concurrent/sequential streams don't steal chunks.
        let (promise, rr) = Promise::create(ctx)?;
        cp.pending_read.borrow_mut().replace([rr[0], rr[1]]);
        let _ = cp.call_controller_pull(ctx);
        return Ok(promise.into());
    }

    // No controller (e.g. detached) — nothing to read.
    let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
    make_resolved_promise(ctx, result)
}

fn js_reader_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let reason = cc.arg(0);
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;
    match &p.stream {
        Some(stream_val) => readable_stream_cancel(ctx, &JSValue::from(*stream_val), reason),
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

// ============================================================================
// ReadableStreamDefaultController
// ============================================================================

pub(crate) struct ReadableStreamDefaultControllerPayload {
    pub(crate) stream: Option<JSReadableStream>,
    pub(crate) underlying_source: JSValue,
    pub(crate) queue: Rc<RefCell<Vec<QueueEntry>>>,
    pub(crate) queue_total_size: Rc<Cell<f64>>,
    pub(crate) started: Cell<bool>,
    pub(crate) close_requested: Cell<bool>,
    pub(crate) pull_again: Cell<bool>,
    pub(crate) pulling: Cell<bool>,
    /// The single in-flight read() resolver awaiting data (resolve, reject).
    /// Stored per-controller (not globally) so concurrent/sequential streams
    /// do not steal each other's pending chunk.
    pub(crate) pending_read: Rc<RefCell<Option<[JSValue; 2]>>>,
    /// Self JS handle, set right after instance creation, used to re-queue
    /// pull jobs.
    pub(crate) controller: Option<JSReadableStreamDefaultController>,
}

impl ReadableStreamDefaultControllerPayload {
    pub(crate) fn new(underlying_source: JSValue, stream: Option<JSReadableStream>) -> Self {
        Self {
            stream,
            underlying_source,
            queue: Rc::new(RefCell::new(Vec::new())),
            queue_total_size: Rc::new(Cell::new(0.0)),
            started: Cell::new(false),
            close_requested: Cell::new(false),
            pull_again: Cell::new(false),
            pulling: Cell::new(false),
            pending_read: Rc::new(RefCell::new(None)),
            controller: None,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_source.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        for entry in self.queue.borrow().iter() {
            entry.chunk.trace(gcx);
        }
        if let Some(rr) = self.pending_read.borrow().as_ref() {
            rr[0].trace(gcx);
            rr[1].trace(gcx);
        }
    }

    /// Resolve the controller's pending read (if any) with an iterator result
    /// `{value, done}`. Returns true if a pending read was present and resolved.
    pub(super) fn resolve_pending_read(
        &self,
        ctx: &mut ScopedContext,
        value: JSValue,
        done: bool,
    ) -> bool {
        let rr = self.pending_read.borrow_mut().take();
        if let Some([resolve, _reject]) = rr {
            if let Ok(result) = make_iterator_result(ctx, value, done) {
                let _ = resolve.call(ctx, JSValue::UNDEFINED, &[result]);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Reject the controller's pending read, if any.
    pub(super) fn reject_pending_read(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let rr = self.pending_read.borrow_mut().take();
        if let Some([_resolve, reject]) = rr {
            let _ = reject.call(ctx, JSValue::UNDEFINED, &[reason]);
        }
    }

    /// Ask the controller to pull one more chunk (call its underlying source's
    /// pull, or refill the queue if strategy is undefined).
    pub(super) fn call_controller_pull(&self, ctx: &mut ScopedContext) -> Result<(), i32> {
        if self.pulling.get() {
            self.pull_again.set(true);
            return Ok(());
        }

        self.pulling.set(true);
        self.started.set(true);

        let source = &self.underlying_source;
        if source.is_undefined() {
            self.pulling.set(false);
            return Ok(());
        }

        let obj = match source.ensure_object(ctx) {
            Ok(o) => o,
            Err(_) => {
                self.pulling.set(false);
                return Ok(());
            }
        };

        let pull_fn = match obj.get_property_str(ctx, "pull") {
            Ok(v) if v.is_callable() => v,
            _ => {
                self.pulling.set(false);
                return Ok(());
            }
        };

        let controller_val = self
            .controller
            .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;
        let _ = pull_fn.call(ctx, *source, &[JSValue::from(controller_val)]);

        self.pulling.set(false);

        if self.pull_again.get() {
            self.pull_again.set(false);
            let ctrl = controller_val;
            ctx.queue_job(
                move |cx| {
                    let p = ctrl
                        .payload()
                        .ok_or_else(|| cx.throw_internal_error("invalid controller"))?;
                    p.call_controller_pull(cx)?;
                    Ok(JobState::Done)
                },
                (),
            );
        }

        Ok(())
    }
}

const CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("enqueue", 1, js_controller_enqueue, 0),
    FieldDef::FUNC("close", 0, js_controller_close, 0),
    FieldDef::FUNC("error", 1, js_controller_error, 0),
    FieldDef::GETSET("desiredSize", Some(js_controller_desired_size), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableStreamDefaultController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("ReadableStreamDefaultController cannot be constructed directly"))
}

def_native_object!(
    "ReadableStreamDefaultController",
    JSReadableStreamDefaultController,
    ReadableStreamDefaultControllerPayload,
    CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableStreamDefaultControllerPayload::gc_trace
);

fn js_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    if p.close_requested.get() {
        return Err(ctx.throw_type_error("cannot enqueue after close"));
    }

    let resolved = p.resolve_pending_read(ctx, chunk, false);

    if !resolved {
        p.queue.borrow_mut().push(QueueEntry { chunk, size: 1.0 });
        let total = p.queue_total_size.get();
        p.queue_total_size.set(total + 1.0);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    p.close_requested.set(true);

    p.resolve_pending_read(ctx, JSValue::UNDEFINED, true);

    // Per spec: only transition to closed when queue is empty.
    // If queue still has items, state stays "readable" until last
    // item is consumed by read().
    if p.queue.borrow().is_empty()
        && let Some(stream_val) = p.stream
    {
        settle_stream_state(ctx, stream_val.into(), RsState::Closed, None);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    p.reject_pending_read(ctx, e);

    if let Some(stream_val) = p.stream {
        settle_stream_state(ctx, stream_val.into(), RsState::Errored, Some(e));
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;
    let total = p.queue_total_size.get();
    Ok(JSValue::from(1.0 - total))
}

// ============================================================================
// Controller setup helpers
// ============================================================================

fn create_readable_stream_controller(
    ctx: &mut ScopedContext,
    stream: &JSReadableStream,
    underlying_source: &JSValue,
) -> Result<JSReadableStreamDefaultController, i32> {
    let mut controller = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(
            if underlying_source.is_undefined_or_null() {
                JSValue::UNDEFINED
            } else {
                *underlying_source
            },
            Some(*stream),
        ),
    )?;
    let handle = controller;
    controller
        .payload_mut()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
        .controller = Some(handle);
    Ok(controller)
}

// ============================================================================
// Stream cancel & async iterator
// ============================================================================

fn readable_stream_cancel(ctx: &mut ScopedContext, stream: &JSValue, reason: JSValue) -> JSResult {
    if let Some(mut stream_obj) = stream.as_native_object::<JSReadableStream>() {
        // Spec: cancel() sets disturbed, so the stream is no longer usable as
        // a body even though no read() ever happened.
        if let Some(p) = stream_obj.payload_mut() {
            p.disturbed = true;
        }
        settle_stream_state(ctx, (*stream_obj).into(), RsState::Closed, None);

        if let Some(p) = stream_obj.payload()
            && let Some(controller_val) = p.controller
            && let Some(cp) = controller_val.payload()
        {
            let source = cp.underlying_source;
            let _ = call_opt_method(ctx, source, "cancel", &[reason]);
        }
    }

    make_resolved_promise(ctx, JSValue::UNDEFINED)
}

fn create_stream_async_iterator(ctx: &mut ScopedContext, stream_val: &JSValue) -> JSResult {
    // Lock the stream by creating a reader.
    let reader_val = {
        let p = JSReadableStream::payload_of(stream_val)
            .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;
        if p.reader.is_some() {
            return Err(
                ctx.throw_type_error("ReadableStream is locked — cannot create async iterator")
            );
        }

        let mut this = *stream_val;
        let reader = JSReadableStreamDefaultReader::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultReaderPayload {
                stream: Some(JSReadableStream::try_from(*stream_val)?),
                closed_promise: None,
                byob: false,
            },
        )?;
        if let Some(sp) = JSReadableStream::payload_mut_of(&mut this) {
            sp.reader = Some(reader);
        }
        JSValue::from(reader)
    };

    struct ReaderFetcher {
        reader_val: JSReadableStreamDefaultReader,
        closed: Cell<bool>,
        // Cache the promise from reader.read() so we don't call reader.read()
        // again on every tick, which would create a new PENDING_READ and lose
        // the value committed by a previous enqueue().
        cached_promise: RefCell<Option<Promise>>,
    }

    impl AsyncIteratorFetcher for ReaderFetcher {
        fn try_fetch(&self, ctx: &mut ScopedContext) -> Result<IterFetchStatus, i32> {
            if self.closed.get() {
                return Ok(IterFetchStatus::Done);
            }

            // Check cached promise first (from a previous try_fetch call).
            // The cached promise is moved out under a single borrow; for the
            // Pending case we put it back before returning so the same borrow
            // is never held across a second borrow_mut() call.
            let cached = self.cached_promise.borrow_mut().take();
            if let Some(promise) = cached {
                match promise.state() {
                    zuqe::builtins::promise::PromiseState::Fulfilled => {
                        let result = promise.result();
                        if let Ok(done_val) = result.get_property_str(ctx, "done")
                            && done_val.to_bool()?
                        {
                            return Ok(IterFetchStatus::Done);
                        }
                        if let Ok(value) = result.get_property_str(ctx, "value") {
                            return Ok(IterFetchStatus::Value(value));
                        }
                        return Ok(IterFetchStatus::Value(result));
                    }
                    zuqe::builtins::promise::PromiseState::Rejected => {
                        return Err(ctx.throw_type_error("stream read rejected"));
                    }
                    zuqe::builtins::promise::PromiseState::Pending => {
                        // Still pending — put it back and wait. Borrow is now
                        // released (promise is owned locally), so this is safe.
                        *self.cached_promise.borrow_mut() = Some(promise);
                        return Ok(IterFetchStatus::Wait);
                    }
                }
            }

            // No cached promise — make a fresh reader.read() call
            let reader_val = JSValue::from(self.reader_val);
            let read_fn = reader_val.get_property_str(ctx, "read")?;
            if !read_fn.is_callable() {
                return Ok(IterFetchStatus::Done);
            }

            let promise_val = read_fn.call(ctx, reader_val, &[])?;
            if !promise_val.is_object() {
                return Ok(IterFetchStatus::Done);
            }

            if let Ok(promise) = Promise::try_from(promise_val) {
                match promise.state() {
                    zuqe::builtins::promise::PromiseState::Fulfilled => {
                        let result = promise.result();
                        if let Ok(done_val) = result.get_property_str(ctx, "done")
                            && done_val.to_bool()?
                        {
                            return Ok(IterFetchStatus::Done);
                        }
                        if let Ok(value) = result.get_property_str(ctx, "value") {
                            return Ok(IterFetchStatus::Value(value));
                        }
                        Ok(IterFetchStatus::Value(result))
                    }
                    zuqe::builtins::promise::PromiseState::Rejected => {
                        Err(ctx.throw_type_error("stream read rejected"))
                    }
                    zuqe::builtins::promise::PromiseState::Pending => {
                        // Cache the pending promise and wait for enqueue() to resolve it
                        *self.cached_promise.borrow_mut() = Some(promise);
                        Ok(IterFetchStatus::Wait)
                    }
                }
            } else {
                Ok(IterFetchStatus::Done)
            }
        }

        fn close(&self) {
            self.closed.set(true);
            // The polling job checks `closed` and returns Done, so the cached
            // promise is the only thing that needs clearing. The in-flight read
            // is stored on the stream's controller (per-controller, not global),
            // and is released by GC once the iterator is dropped.
            *self.cached_promise.borrow_mut() = None;
        }
    }

    let iter = AsyncIterableIterator::create(
        ctx,
        ReaderFetcher {
            reader_val: JSReadableStreamDefaultReader::try_from(reader_val).map_err(|_| -1i32)?,
            closed: Cell::new(false),
            cached_promise: RefCell::new(None),
        },
    )
    .map_err(|_| -1i32)?;

    Ok(iter.into())
}

// ============================================================================
// Channel/bytes-backed streams
// ============================================================================

/// Create a `ReadableStream` backed by a `crossbeam::Receiver<Result<Vec<u8>, String>>`.
///
/// Uses a background microtask (queue_job) to poll the channel and push
/// chunks into the stream's controller. A `Ok(bytes)` item enqueues a chunk;
/// an `Err(msg)` item errors the stream (the reader's `read()` rejects); when
/// the channel is disconnected the stream is closed.
pub(crate) fn readable_stream_from_channel(
    ctx: &mut ScopedContext,
    rx: Receiver<Result<Vec<u8>, String>>,
    signal: Option<JSValue>,
) -> Result<JSValue, i32> {
    let source = ctx.create_base_object()?;

    let mut stream =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;

    let controller = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream)),
    )?;
    stream.payload_mut().unwrap().controller = Some(controller);

    let id = NEXT_STREAM_ID.with(|n| {
        let cur = n.get();
        n.set(cur + 1);
        cur
    });

    // Keep the abort signal alive on the stream payload (GC-traced), not just in
    // the channel registry, so it cannot be reclaimed while the body is live.
    let mut sp = stream.payload_mut().unwrap();
    sp.signal = signal;
    sp.channel_id = Some(id);

    let was_empty = CHANNEL_STREAMS.with(|map| {
        let empty = map.borrow().is_empty();
        map.borrow_mut()
            .insert(id, (controller, rx, Instant::now()));
        empty
    });

    if was_empty {
        start_channel_stream_poller(ctx);
    }

    Ok(JSValue::from(stream))
}

/// Create a `ReadableStream` from a single byte buffer (synthetic body).
///
/// Unlike `readable_stream_from_channel`, this does not create or poll a
/// channel — the data is enqueued and the stream is closed immediately.
pub(crate) fn readable_stream_from_bytes(
    ctx: &mut ScopedContext,
    bytes: Vec<u8>,
) -> Result<JSValue, i32> {
    let source = ctx.create_base_object()?;

    let mut stream =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;

    let controller = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream)),
    )?;
    let controller_val = JSValue::from(controller);

    stream.payload_mut().unwrap().controller = Some(controller);

    // Enqueue the single chunk as a TypedArray.
    let ta = TypedArray::create_from_vec_u8(ctx, bytes)?;
    let enqueue_fn = controller_val.get_property_str(ctx, "enqueue")?;
    let _ = enqueue_fn.call(ctx, controller_val, &[JSValue::from(ta)]);

    // Close the stream — data is the full body.
    let close_fn = controller_val.get_property_str(ctx, "close")?;
    let _ = close_fn.call(ctx, controller_val, &[]);

    Ok(JSValue::from(stream))
}

/// Create a `ReadableStream` from an already-decoded text string. Non-empty
/// text is enqueued as a single string chunk; empty text produces a closed,
/// chunk-free stream. Used by Request/Response `textStream()`.
pub(crate) fn readable_stream_from_text(
    ctx: &mut ScopedContext,
    text: String,
) -> Result<JSValue, i32> {
    let source = ctx.create_base_object()?;
    let mut stream =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
    let controller = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream)),
    )?;
    let controller_val = JSValue::from(controller);
    stream.payload_mut().unwrap().controller = Some(controller);

    if !text.is_empty() {
        let sval = ctx.create_string_from_utf8(text.into_bytes())?;
        let enqueue_fn = controller_val.get_property_str(ctx, "enqueue")?;
        let _ = enqueue_fn.call(ctx, controller_val, &[JSValue::make_string(sval)]);
    }
    // Close the stream — the text is the entire body.
    let close_fn = controller_val.get_property_str(ctx, "close")?;
    let _ = close_fn.call(ctx, controller_val, &[]);

    Ok(JSValue::from(stream))
}

/// Create a `ReadableStream` of UTF-8-decoded string chunks from a byte
/// channel. Each `Ok(bytes)` item is decoded and enqueued as a string; an
/// `Err(msg)` item errors the stream; disconnect closes it. Used by
/// `textStream()` on network responses.
pub(crate) fn readable_stream_from_text_channel(
    ctx: &mut ScopedContext,
    rx: Receiver<Result<Vec<u8>, String>>,
) -> Result<JSValue, i32> {
    let source = ctx.create_base_object()?;
    let mut stream =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
    let controller = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream)),
    )?;
    stream.payload_mut().unwrap().controller = Some(controller);

    let ctrl = controller;
    let start = Instant::now();
    const TEXT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
    ctx.queue_job(
        move |cx| match rx.try_recv() {
            Ok(Ok(bytes)) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let sval = cx.create_string_from_utf8(text.into_bytes())?;
                let enqueue_fn = ctrl.get_property_str(cx, "enqueue")?;
                let _ = enqueue_fn.call(cx, JSValue::from(ctrl), &[JSValue::make_string(sval)]);
                Ok(JobState::Again)
            }
            Ok(Err(msg)) => {
                let err = cx
                    .create_error_object(ErrorKind::Type, &msg, false)
                    .ok_or(-1)?;
                let error_fn = ctrl.get_property_str(cx, "error")?;
                let _ = error_fn.call(cx, JSValue::from(ctrl), &[err.into()]);
                Ok(JobState::Done)
            }
            Err(TryRecvError::Disconnected) => {
                let close_fn = ctrl.get_property_str(cx, "close")?;
                let _ = close_fn.call(cx, JSValue::from(ctrl), &[]);
                Ok(JobState::Done)
            }
            Err(TryRecvError::Empty) => {
                if start.elapsed() > TEXT_STREAM_IDLE_TIMEOUT {
                    let err = cx.create_string_from_utf8(b"text stream timeout".to_vec())?;
                    let error_fn = ctrl.get_property_str(cx, "error")?;
                    let _ = error_fn.call(cx, JSValue::from(ctrl), &[JSValue::make_string(err)]);
                    Ok(JobState::Done)
                } else {
                    Ok(JobState::Again)
                }
            }
        },
        (),
    );

    Ok(JSValue::from(stream))
}

fn start_channel_stream_poller(ctx: &mut ScopedContext) {
    // Idle timeout, applied per stream and refreshed on every received chunk.
    // A stream is only killed when its own producer goes quiet for this long,
    // so neither a slow body nor a long-lived sibling stream can starve it.
    const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

    ctx.queue_job(
        move |cx| {
            CHANNEL_STREAMS.with(|map| {
                // Snapshot ids first, then release the borrow before any JS call.
                // Holding borrow_mut across the loop would (a) panic on a second
                // borrow_mut if a GC fires mid-loop (gc_trace removes entries),
                // and (b) leave a dangling controller in the registry if the
                // stream's payload is collected. Per-id borrowing keeps the map
                // free during JS work.
                let ids: Vec<u64> = map.borrow().keys().cloned().collect();
                let mut to_remove = Vec::new();

                for id in ids {
                    // Resolve abort signal from the GC-traced stream payload
                    // (controller -> stream -> signal). Never read a dangling
                    // handle: only visit entries that still map to a live stream.
                    let resolved = {
                        let m = map.borrow();
                        match m.get(&id) {
                            Some((ctrl, _, _)) => {
                                let maybe_signal = JSReadableStreamDefaultController::payload_of(
                                    &JSValue::from(*ctrl),
                                )
                                .and_then(|cp| cp.stream)
                                .and_then(|stream| {
                                    JSReadableStream::payload_of(&JSValue::from(stream))
                                        .map(|sp| sp.signal)
                                })
                                .flatten();
                                Some((JSValue::from(*ctrl), maybe_signal))
                            }
                            None => None,
                        }
                    };
                    let (controller, signal_opt) = match resolved {
                        Some(v) => v,
                        None => continue,
                    };

                    // Abort check before draining data: abort wins over a chunk.
                    if let Some(s) = signal_opt {
                        if signal_is_aborted(&s) {
                            let reason = signal_reason(&s).unwrap_or_else(|| {
                                make_dom_exception(cx, "AbortError", "The operation was aborted.")
                                    .unwrap_or(JSValue::UNDEFINED)
                            });
                            if let Ok(ef) = controller.get_property_str(cx, "error") {
                                let _ = ef.call(cx, JSValue::from(controller), &[reason]);
                            }
                            to_remove.push(id);
                            continue;
                        }
                    }

                    // Re-borrow briefly to receive from the channel, releasing
                    // before any JS call below.
                    let recv = map.borrow().get(&id).map(|(_, rx, _)| rx.try_recv());
                    match recv {
                        Some(Ok(Ok(data))) => {
                            // Producer alive: refresh deadline, then enqueue.
                            if let Some(d) = map.borrow_mut().get_mut(&id) {
                                d.2 = Instant::now();
                            }
                            let _ = (|| -> Result<(), i32> {
                                let ta =
                                    TypedArray::create_from_vec_u8(cx, data).map_err(|_| {
                                        cx.throw_internal_error("TypedArray creation failed")
                                    })?;
                                let enqueue_fn = controller.get_property_str(cx, "enqueue")?;
                                let _ = enqueue_fn.call(
                                    cx,
                                    JSValue::from(controller),
                                    &[JSValue::from(ta)],
                                );
                                Ok(())
                            })();
                        }
                        Some(Ok(Err(msg))) => {
                            // Underlying network stream failed mid-body: error.
                            let _ = (|| -> Result<(), i32> {
                                let bytes = msg.into_bytes();
                                let err = cx
                                    .create_error_object(
                                        ErrorKind::Type,
                                        &String::from_utf8_lossy(&bytes),
                                        false,
                                    )
                                    .ok_or(-1)?;
                                let error_fn = controller.get_property_str(cx, "error")?;
                                let _ = error_fn.call(cx, JSValue::from(controller), &[err.into()]);
                                Ok(())
                            })();
                            to_remove.push(id);
                        }
                        Some(Err(TryRecvError::Disconnected)) => {
                            let _ = (|| -> Result<(), i32> {
                                let close_fn = controller.get_property_str(cx, "close")?;
                                let _ = close_fn.call(cx, JSValue::from(controller), &[]);
                                Ok(())
                            })();
                            to_remove.push(id);
                        }
                        Some(Err(TryRecvError::Empty)) => {
                            let expired = map
                                .borrow()
                                .get(&id)
                                .map(|(_, _, d)| d.elapsed() > STREAM_IDLE_TIMEOUT)
                                .unwrap_or(true);
                            if expired {
                                let _ = (|| -> Result<(), i32> {
                                    let err = cx
                                        .create_string_from_utf8(b"channel stream timeout".to_vec())
                                        .map(JSValue::make_string)
                                        .map_err(|_| -1)?;
                                    let error_fn = controller.get_property_str(cx, "error")?;
                                    let _ = error_fn.call(cx, JSValue::from(controller), &[err]);
                                    Ok(())
                                })();
                                to_remove.push(id);
                            }
                        }
                        None => {}
                    }
                }

                if !to_remove.is_empty() {
                    let mut m = map.borrow_mut();
                    for id in &to_remove {
                        m.remove(id);
                    }
                }
            });

            if CHANNEL_STREAMS.with(|map| map.borrow().is_empty()) {
                Ok(JobState::Done)
            } else {
                Ok(JobState::Again)
            }
        },
        (),
    );
}

// ============================================================================
// Registration
// ============================================================================

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStream::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream_default_reader(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStreamDefaultReader::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStreamDefaultController::register(rt)
}
