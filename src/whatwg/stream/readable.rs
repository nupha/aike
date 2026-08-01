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
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, JobState, Runtime,
    ScopedContext,
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

pub(crate) struct ReadableStreamPayload {
    pub(crate) state: RsState,
    pub(crate) stored_error: JSValue,
    pub(crate) reader: Option<JSReadableStreamDefaultReader>,
    pub(crate) controller: Option<JSReadableStreamDefaultController>,
    /// Set once read() is called. Unlike the reader lock this is never
    /// cleared by releaseLock(), so a used stream stays unusable as a body.
    pub(crate) disturbed: bool,
}

impl ReadableStreamPayload {
    pub(crate) fn new() -> Self {
        Self {
            state: RsState::Readable,
            stored_error: JSValue::UNDEFINED,
            reader: None,
            controller: None,
            disturbed: false,
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
}

thread_local! {
    /// Channel-backed streams being polled: id -> (controller, receiver).
    pub(super) static CHANNEL_STREAMS: RefCell<HashMap<u64, (JSReadableStreamDefaultController, Receiver<Vec<u8>>)>> =
        RefCell::new(HashMap::new());

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

fn js_reader_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultReader::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;

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

            // Per spec: after draining last item, if close_requested,
            // transition stream to closed state.
            if q.is_empty()
                && cp.close_requested.get()
                && let Some(sp) = stream_val.payload_mut()
            {
                sp.state = RsState::Closed;
            }

            let result = make_iterator_result(ctx, entry.chunk, false)?;
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

            // Check cached promise first (from a previous try_fetch call)
            if let Some(promise) = self.cached_promise.borrow_mut().take() {
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
                        // Still pending — put it back and wait
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

/// Create a `ReadableStream` backed by a `crossbeam::Receiver<Vec<u8>>`.
///
/// Uses a background microtask (queue_job) to poll the channel and push
/// chunks into the stream's controller. When the channel is disconnected
/// the stream is closed.
pub(crate) fn readable_stream_from_channel(
    ctx: &mut ScopedContext,
    rx: Receiver<Vec<u8>>,
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

    let was_empty = CHANNEL_STREAMS.with(|map| {
        let empty = map.borrow().is_empty();
        map.borrow_mut().insert(id, (controller, rx));
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

fn start_channel_stream_poller(ctx: &mut ScopedContext) {
    let start = Instant::now();
    const POLLER_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_TICKS: u32 = 100_000;
    let tick_count = Cell::new(0u32);

    ctx.queue_job(
        move |cx| {
            let count = tick_count.get();
            tick_count.set(count + 1);
            let timed_out = start.elapsed() > POLLER_TIMEOUT || count > MAX_TICKS;

            CHANNEL_STREAMS.with(|map| {
                let mut map = map.borrow_mut();
                let mut to_remove = Vec::new();

                for (&id, (controller, rx)) in map.iter() {
                    if timed_out {
                        let _ = (|| -> Result<(), i32> {
                            let err = cx
                                .create_string_from_utf8(b"channel stream timeout".to_vec())
                                .map(JSValue::make_string)
                                .map_err(|_| -1)?;
                            let error_fn = controller.get_property_str(cx, "error")?;
                            let _ = error_fn.call(cx, JSValue::from(*controller), &[err]);
                            Ok(())
                        })();
                        to_remove.push(id);
                        continue;
                    }

                    match rx.try_recv() {
                        Ok(data) => {
                            let _ = (|| -> Result<(), i32> {
                                let ta =
                                    TypedArray::create_from_vec_u8(cx, data).map_err(|_| {
                                        cx.throw_internal_error("TypedArray creation failed")
                                    })?;
                                let enqueue_fn = controller.get_property_str(cx, "enqueue")?;
                                let _ = enqueue_fn.call(
                                    cx,
                                    JSValue::from(*controller),
                                    &[JSValue::from(ta)],
                                );
                                Ok(())
                            })();
                        }
                        Err(TryRecvError::Disconnected) => {
                            let _ = (|| -> Result<(), i32> {
                                let close_fn = controller.get_property_str(cx, "close")?;
                                let _ = close_fn.call(cx, JSValue::from(*controller), &[]);
                                Ok(())
                            })();
                            to_remove.push(id);
                        }
                        Err(TryRecvError::Empty) => {}
                    }
                }

                for id in &to_remove {
                    map.remove(id);
                }
            });

            if timed_out || CHANNEL_STREAMS.with(|map| map.borrow().is_empty()) {
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
