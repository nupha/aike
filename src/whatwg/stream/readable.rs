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
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, JobState, Runtime,
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
    PENDING_READ, QueueEntry, RsState, call_opt_method, make_iterator_result,
    make_rejected_promise, make_resolved_promise, reject_pending_read, resolve_pending_read,
    settle_stream_state,
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

fn js_readable_stream_pipe_to(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_internal_error("pipeTo not yet implemented"))
}

fn js_readable_stream_pipe_through(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_internal_error("pipeThrough not yet implemented"))
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
    }

    // No data — create a pending promise. Store resolving functions for later invocation.
    let (promise, rr) = Promise::create(ctx)?;
    PENDING_READ.with(|slot| *slot.borrow_mut() = Some((promise, rr)));

    if let Some(controller_val) = stream_p.controller {
        let _ = call_controller_pull(ctx, controller_val);
    }

    Ok(promise.into())
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

    let resolved = PENDING_READ.with(|slot| {
        let mut s = slot.borrow_mut();
        if let Some((_promise, rr)) = s.take() {
            let result = make_iterator_result(ctx, chunk, false);
            match result {
                Ok(r) => {
                    let _ = rr[0].call(ctx, JSValue::UNDEFINED, &[r]);
                    true
                }
                Err(_) => false,
            }
        } else {
            false
        }
    });

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

    resolve_pending_read(ctx, JSValue::UNDEFINED, true);

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

    reject_pending_read(ctx, e);

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
    JSReadableStreamDefaultController::new_instance(
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
    )
}

fn call_controller_pull(
    ctx: &mut ScopedContext,
    controller: JSReadableStreamDefaultController,
) -> Result<(), i32> {
    let p = controller
        .payload()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

    if p.pulling.get() {
        p.pull_again.set(true);
        return Ok(());
    }

    p.pulling.set(true);
    p.started.set(true);

    let source = &p.underlying_source;
    if source.is_undefined() {
        p.pulling.set(false);
        return Ok(());
    }

    let obj = match source.ensure_object(ctx) {
        Ok(o) => o,
        Err(_) => {
            p.pulling.set(false);
            return Ok(());
        }
    };

    let pull_fn = match obj.get_property_str(ctx, "pull") {
        Ok(v) if v.is_callable() => v,
        _ => {
            p.pulling.set(false);
            return Ok(());
        }
    };

    let _ = pull_fn.call(ctx, *source, &[JSValue::from(controller)]);

    p.pulling.set(false);

    if p.pull_again.get() {
        p.pull_again.set(false);
        let ctrl = controller;
        ctx.queue_job(
            move |cx| {
                let _ = call_controller_pull(cx, ctrl);
                Ok(JobState::Done)
            },
            (),
        );
    }

    Ok(())
}

// ============================================================================
// Stream cancel & async iterator
// ============================================================================

fn readable_stream_cancel(ctx: &mut ScopedContext, stream: &JSValue, reason: JSValue) -> JSResult {
    if let Some(stream_obj) = stream.as_native_object::<JSReadableStream>() {
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
            // Clear any dangling PENDING_READ and cached promise so the
            // polling job from the last next() call can terminate cleanly
            // after the async iterator's return() is invoked.
            PENDING_READ.with(|slot| {
                *slot.borrow_mut() = None;
            });
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
