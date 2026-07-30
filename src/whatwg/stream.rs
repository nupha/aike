// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Streams standard implementation.
//!
//! Implements ReadableStream, WritableStream, TransformStream and related
//! classes per the [WHATWG Streams specification](https://streams.spec.whatwg.org/).

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
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

#[derive(Clone, Copy, PartialEq)]
enum RsState {
    Readable,
    Closed,
    Errored,
}

struct QueueEntry {
    chunk: JSValue,
    size: f64,
}

struct ReadableStreamPayload {
    state: RsState,
    stored_error: JSValue,
    reader: Option<JSReadableStreamDefaultReader>,
    controller: Option<JSReadableStreamDefaultController>,
}

impl ReadableStreamPayload {
    fn new() -> Self {
        Self {
            state: RsState::Readable,
            stored_error: JSValue::UNDEFINED,
            reader: None,
            controller: None,
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

    if let Some(obj) = underlying_source.as_object()
        && let Ok(start_fn) = obj.get_property_str(ctx, "start")
        && start_fn.is_callable()
    {
        let _ = start_fn.call(ctx, underlying_source, &[JSValue::from(controller)]);
    }

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
    closed_promise: Option<Promise>,
}

thread_local! {
    static PENDING_READ: RefCell<Option<(Promise, [JSValue; 2])>> =
        const { RefCell::new(None) };

    static CHANNEL_STREAMS: RefCell<HashMap<u64, (JSReadableStreamDefaultController, Receiver<Vec<u8>>)>> =
        RefCell::new(HashMap::new());

    static NEXT_STREAM_ID: Cell<u64> = const { Cell::new(1) };
}

/// Called by the runtime GC to keep PENDING_READ's JSValues alive.
pub(crate) fn gc_trace_stream(gcx: &mut GcTraceCtx) {
    PENDING_READ.with_borrow(|opt| {
        if let Some((promise, rr)) = opt.as_ref() {
            promise.trace(gcx);
            rr[0].trace(gcx);
            rr[1].trace(gcx);
        }
    });

    CHANNEL_STREAMS.with_borrow(|map| {
        for (controller, _) in map.values() {
            controller.as_object().trace(gcx);
        }
    });
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
    stream: Option<JSReadableStream>,
    underlying_source: JSValue,
    queue: Rc<RefCell<Vec<QueueEntry>>>,
    queue_total_size: Rc<Cell<f64>>,
    started: Cell<bool>,
    close_requested: Cell<bool>,
    pull_again: Cell<bool>,
    pulling: Cell<bool>,
}

impl ReadableStreamDefaultControllerPayload {
    fn new(underlying_source: JSValue, stream: Option<JSReadableStream>) -> Self {
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

    PENDING_READ.with(|slot| {
        let mut s = slot.borrow_mut();
        if let Some((_promise, rr)) = s.take()
            && let Ok(result) = make_iterator_result(ctx, JSValue::UNDEFINED, true)
        {
            let _ = rr[0].call(ctx, JSValue::UNDEFINED, &[result]);
        }
    });

    // Per spec: only transition to closed when queue is empty.
    // If queue still has items, state stays "readable" until last
    // item is consumed by read().
    if p.queue.borrow().is_empty()
        && let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        sp.state = RsState::Closed;
        if let Some(reader_val) = sp.reader {
            if let Some(rp) = reader_val.payload() {
                if let Some(cp) = rp.closed_promise {
                    let mut cp = cp;
                    cp.resolve(ctx, JSValue::UNDEFINED);
                }
            }
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    PENDING_READ.with(|slot| {
        let mut s = slot.borrow_mut();
        if let Some((mut promise, _slot)) = s.take() {
            promise.reject(ctx, e);
        }
    });

    if let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        sp.state = RsState::Errored;
        sp.stored_error = e;
        if let Some(reader_val) = sp.reader {
            if let Some(rp) = reader_val.payload() {
                if let Some(cp) = rp.closed_promise {
                    let mut cp = cp;
                    cp.reject(ctx, e);
                }
            }
        }
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
    if let Some(mut stream_obj) = stream.as_native_object::<JSReadableStream>()
        && let Some(p) = stream_obj.payload_mut()
    {
        p.state = RsState::Closed;

        if let Some(reader_val) = p.reader {
            if let Some(rp) = reader_val.payload() {
                if let Some(mut cp) = rp.closed_promise {
                    cp.resolve(ctx, JSValue::UNDEFINED);
                }
            }
        }

        if let Some(controller_val) = p.controller
            && let Some(cp) = controller_val.payload()
        {
            let source = &cp.underlying_source;
            if let Some(obj) = source.as_object()
                && let Ok(cancel_fn) = obj.get_property_str(ctx, "cancel")
                && cancel_fn.is_callable()
            {
                let _ = cancel_fn.call(ctx, *source, &[reason]);
            }
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
// WritableStream
// ============================================================================

pub(crate) struct WritableStreamPayload {
    state: RsState,
    stored_error: JSValue,
    writer: Option<JSWritableStreamDefaultWriter>,
    controller: Option<JSWritableStreamDefaultController>,
}

impl WritableStreamPayload {
    fn new() -> Self {
        Self {
            state: RsState::Readable,
            stored_error: JSValue::UNDEFINED,
            writer: None,
            controller: None,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.stored_error.trace(gcx);
        if let Some(ref w) = self.writer {
            w.trace(gcx);
        }
        if let Some(ref c) = self.controller {
            c.trace(gcx);
        }
    }
}

const WRITABLE_STREAM_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("locked", Some(js_writable_stream_locked), None, 0),
    FieldDef::FUNC("getWriter", 0, js_writable_stream_get_writer, 0),
    FieldDef::FUNC("abort", 1, js_writable_stream_abort, 0),
    FieldDef::FUNC("close", 0, js_writable_stream_close, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "WritableStream",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "WritableStream",
    JSWritableStream,
    WritableStreamPayload,
    WRITABLE_STREAM_PROTO,
    Some(ClassCtorDef::new(js_writable_stream_constructor, 1, false).expose_to_global()),
    on_payload_gc: WritableStreamPayload::gc_trace
);

fn js_writable_stream_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let underlying_sink = cc.arg(0);
    let _queuing_strategy = cc.arg(1);

    let mut stream = JSWritableStream::new_instance(ctx, new_target, WritableStreamPayload::new())?;

    let controller = JSWritableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        WritableStreamDefaultControllerPayload::new(underlying_sink, Some(stream)),
    )?;

    stream.payload_mut().unwrap().controller = Some(controller);

    if let Some(obj) = underlying_sink.as_object()
        && let Ok(start_fn) = obj.get_property_str(ctx, "start")
        && start_fn.is_callable()
    {
        let _ = start_fn.call(ctx, underlying_sink, &[JSValue::from(controller)]);
    }

    Ok(stream.into())
}

fn js_writable_stream_locked(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream"))?;
    Ok(JSValue::from(p.writer.is_some()))
}

fn js_writable_stream_get_writer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let mut stream = this
        .as_native_object::<JSWritableStream>()
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream"))?;

    let p = stream.payload_mut().unwrap();
    if p.writer.is_some() {
        return Err(ctx.throw_type_error("WritableStream is already locked"));
    }

    let mut closed_promise = Promise::create(ctx)?.0;
    match p.state {
        RsState::Closed => closed_promise.resolve(ctx, JSValue::UNDEFINED),
        RsState::Errored => closed_promise.reject(ctx, p.stored_error),
        RsState::Readable => {}
    }

    let writer = JSWritableStreamDefaultWriter::new_instance(
        ctx,
        JSValue::UNDEFINED,
        WritableStreamDefaultWriterPayload {
            stream: Some(JSWritableStream::try_from(this)?),
            closed_promise: Some(closed_promise),
        },
    )?;

    p.writer = Some(writer);
    Ok(JSValue::from(writer))
}

fn js_writable_stream_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    if let Some(mut stream) = cc.this().as_native_object::<JSWritableStream>()
        && let Some(p) = stream.payload_mut()
    {
        p.state = RsState::Errored;
        p.stored_error = reason;

        if let Some(writer_val) = p.writer {
            if let Some(wp) = writer_val.payload() {
                if let Some(cp) = wp.closed_promise {
                    let mut cp = cp;
                    cp.reject(ctx, reason);
                }
            }
        }

        if let Some(controller) = p.controller
            && let Some(cp) = controller.payload()
            && let Some(obj) = cp.underlying_sink.as_object()
            && let Ok(abort_fn) = obj.get_property_str(ctx, "abort")
            && abort_fn.is_callable()
        {
            // Clear any pending exception left by previous polling jobs
            // before invoking user callback.
            ctx.rt_mut().clear_exception();
            let _ = abort_fn.call(ctx, cp.underlying_sink, &[reason]);
        }
    }
    make_resolved_promise(ctx, JSValue::UNDEFINED)
}

fn js_writable_stream_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    if let Some(mut stream) = cc.this().as_native_object::<JSWritableStream>()
        && let Some(p) = stream.payload_mut()
    {
        p.state = RsState::Closed;
        if let Some(writer_val) = p.writer {
            if let Some(wp) = writer_val.payload() {
                if let Some(cp) = wp.closed_promise {
                    let mut cp = cp;
                    cp.resolve(ctx, JSValue::UNDEFINED);
                }
            }
        }
    }
    make_resolved_promise(ctx, JSValue::UNDEFINED)
}

// ============================================================================
// WritableStreamDefaultWriter
// ============================================================================

pub(crate) struct WritableStreamDefaultWriterPayload {
    stream: Option<JSWritableStream>,
    closed_promise: Option<Promise>,
}

impl WritableStreamDefaultWriterPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref cp) = self.closed_promise {
            cp.trace(gcx);
        }
    }
}

const WRITER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("write", 1, js_writer_write, 0),
    FieldDef::FUNC("close", 0, js_writer_close, 0),
    FieldDef::FUNC("abort", 1, js_writer_abort, 0),
    FieldDef::FUNC("releaseLock", 0, js_writer_release_lock, 0),
    FieldDef::GETSET("closed", Some(js_writer_closed), None, 0),
    FieldDef::GETSET("desiredSize", Some(js_writer_desired_size), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "WritableStreamDefaultWriter",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_writer_closed(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream writer"))?;
    if let Some(cp) = p.closed_promise {
        return Ok(JSValue::from(cp));
    }
    Ok(JSValue::UNDEFINED)
}

fn js_writer_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream writer"))?;
    if let Some(stream_val) = p.stream {
        if let Some(sp) = stream_val.payload() {
            if let Some(ctrl) = sp.controller {
                if let Some(cp) = ctrl.payload() {
                    return Ok(JSValue::from(cp.high_water_mark));
                }
            }
        }
    }
    Ok(JSValue::from(1.0))
}

fn js_writer_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error(
        "WritableStreamDefaultWriter must be obtained via WritableStream.getWriter()",
    ))
}

def_native_object!(
    "WritableStreamDefaultWriter",
    JSWritableStreamDefaultWriter,
    WritableStreamDefaultWriterPayload,
    WRITER_PROTO,
    Some(ClassCtorDef::new(js_writer_constructor, 0, false).expose_to_global()),
    on_payload_gc: WritableStreamDefaultWriterPayload::gc_trace
);

fn js_writer_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.arg(0);
    let this = cc.this();

    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;

    let stream_val = p
        .stream
        .ok_or_else(|| ctx.throw_type_error("writer released"))?;

    if let Some(stream) = stream_val.as_native_object::<JSWritableStream>()
        && let Some(sp) = stream.payload()
    {
        // Reject if stream is errored or closed
        match sp.state {
            RsState::Errored => {
                let reason = sp.stored_error;
                return make_rejected_promise(ctx, reason);
            }
            RsState::Closed => {
                let reason = ctx
                    .create_string_from_utf8(b"Writer is closed".to_vec())
                    .map(JSValue::make_string)
                    .unwrap_or(JSValue::UNDEFINED);
                return make_rejected_promise(ctx, reason);
            }
            _ => {}
        }

        if let Some(controller) = sp.controller {
            let cp = controller
                .payload()
                .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

            // TransformStream path: use transformer.transform(chunk, transformController)
            if let Some(tc) = cp.transform_controller
                && let Some(tcp) = tc.payload()
                && let Some(obj) = tcp.transformer.as_object()
            {
                // Default transform: chunk → readable (pass-through) if no transform fn
                let has_transform = obj
                    .get_property_str(ctx, "transform")
                    .map(|v| v.is_callable())
                    .unwrap_or(false);

                if has_transform {
                    let transform_fn = obj.get_property_str(ctx, "transform").unwrap();
                    let result =
                        transform_fn.call(ctx, tcp.transformer, &[chunk, JSValue::from(tc)])?;
                    if result.is_object() {
                        return Ok(result);
                    }
                } else {
                    // No user-defined transform → pass through: enqueue chunk directly
                    let enqueue_fn =
                        JSValue::from(tcp.readable_controller).get_property_str(ctx, "enqueue")?;
                    enqueue_fn.call(ctx, JSValue::from(tcp.readable_controller), &[chunk])?;
                }
                return make_resolved_promise(ctx, JSValue::UNDEFINED);
            }

            // Normal writable path
            if let Some(obj) = cp.underlying_sink.as_object()
                && let Ok(write_fn) = obj.get_property_str(ctx, "write")
                && write_fn.is_callable()
            {
                let result = write_fn.call(ctx, cp.underlying_sink, &[chunk, controller.into()])?;
                if result.is_object() {
                    return Ok(result);
                }
            }
        }
    }

    make_resolved_promise(ctx, JSValue::UNDEFINED)
}

fn js_writer_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;

    let mut stream_val = p
        .stream
        .ok_or_else(|| ctx.throw_type_error("writer released"))?;

    if let Some(sp) = stream_val.payload_mut() {
        if let Some(controller) = sp.controller
            && let Some(cp) = controller.payload()
        {
            // TransformStream flush path: call transformer.flush(transformController)
            if let Some(tc) = cp.transform_controller
                && let Some(tcp) = tc.payload()
                && let Some(obj) = tcp.transformer.as_object()
            {
                if let Ok(flush_fn) = obj.get_property_str(ctx, "flush")
                    && flush_fn.is_callable()
                {
                    let _ = flush_fn.call(ctx, tcp.transformer, &[JSValue::from(tc)]);
                }

                // After flush, close the readable side of the transform stream
                if let Some(rcp) = tcp.readable_controller.payload() {
                    rcp.close_requested.set(true);

                    PENDING_READ.with(|slot| {
                        let mut s = slot.borrow_mut();
                        if let Some((_promise, rr)) = s.take()
                            && let Ok(result) = make_iterator_result(ctx, JSValue::UNDEFINED, true)
                        {
                            let _ = rr[0].call(ctx, JSValue::UNDEFINED, &[result]);
                        }
                    });

                    if rcp.queue.borrow().is_empty()
                        && let Some(mut stream_val) = rcp.stream
                        && let Some(rsp) = stream_val.payload_mut()
                    {
                        rsp.state = RsState::Closed;
                    }
                }
            } else if let Some(obj) = cp.underlying_sink.as_object()
                && let Ok(close_fn) = obj.get_property_str(ctx, "close")
                && close_fn.is_callable()
            {
                let _ = close_fn.call(ctx, cp.underlying_sink, &[]);
            }
        }

        sp.state = RsState::Closed;
    }

    make_resolved_promise(ctx, JSValue::UNDEFINED)
}

fn js_writer_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    let this = cc.this();

    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;

    if let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        sp.state = RsState::Errored;
        sp.stored_error = reason;

        if let Some(controller) = sp.controller
            && let Some(cp) = controller.payload()
            && let Some(obj) = cp.underlying_sink.as_object()
            && let Ok(abort_fn) = obj.get_property_str(ctx, "abort")
            && abort_fn.is_callable()
        {
            ctx.rt_mut().clear_exception();
            let _ = abort_fn.call(ctx, cp.underlying_sink, &[reason]);
        }
    }

    make_resolved_promise(ctx, JSValue::UNDEFINED)
}

fn js_writer_release_lock(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    if let Some(p) = JSWritableStreamDefaultWriter::payload_mut_of(&mut this)
        && let Some(mut stream_val) = p.stream.take()
        && let Some(sp) = stream_val.payload_mut()
    {
        sp.writer = None;
    }
    Ok(JSValue::UNDEFINED)
}

// ============================================================================
// WritableStreamDefaultController
// ============================================================================

struct WritableStreamDefaultControllerPayload {
    stream: Option<JSWritableStream>,
    underlying_sink: JSValue,
    transform_controller: Option<JSTransformStreamDefaultController>,
    high_water_mark: f64,
}

impl WritableStreamDefaultControllerPayload {
    fn new(underlying_sink: JSValue, stream: Option<JSWritableStream>) -> Self {
        Self {
            stream,
            underlying_sink,
            transform_controller: None,
            high_water_mark: 1.0,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_sink.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref tc) = self.transform_controller {
            tc.trace(gcx);
        }
    }
}

const WRITABLE_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("error", 1, js_writable_controller_error, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "WritableStreamDefaultController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_writable_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("WritableStreamDefaultController cannot be constructed directly"))
}

def_native_object!(
    "WritableStreamDefaultController",
    JSWritableStreamDefaultController,
    WritableStreamDefaultControllerPayload,
    WRITABLE_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_writable_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: WritableStreamDefaultControllerPayload::gc_trace
);

fn js_writable_controller_error(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let e = cc.arg(0);
    let this = cc.this();

    if let Some(p) = JSWritableStreamDefaultController::payload_of(&this)
        && let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        sp.state = RsState::Errored;
        sp.stored_error = e;
    }

    Ok(JSValue::UNDEFINED)
}

// ============================================================================
// TransformStream
// ============================================================================

pub(crate) struct TransformStreamPayload {
    readable: Option<JSReadableStream>,
    writable: Option<JSWritableStream>,
}

impl TransformStreamPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref r) = self.readable {
            r.trace(gcx);
        }
        if let Some(ref w) = self.writable {
            w.trace(gcx);
        }
    }
}

const TRANSFORM_STREAM_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("readable", Some(js_transform_stream_readable), None, 0),
    FieldDef::GETSET("writable", Some(js_transform_stream_writable), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "TransformStream",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "TransformStream",
    JSTransformStream,
    TransformStreamPayload,
    TRANSFORM_STREAM_PROTO,
    Some(ClassCtorDef::new(js_transform_stream_constructor, 1, false).expose_to_global()),
    on_payload_gc: TransformStreamPayload::gc_trace
);

fn js_transform_stream_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let transformer = cc.arg(0);

    // Use existing readable/writable instances, or create an empty transformer object.
    let transformer = if transformer.is_undefined_or_null() {
        ctx.create_base_object().map(JSValue::from)?
    } else {
        transformer
    };

    // Readable side
    let mut readable =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
    let rc = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(JSValue::UNDEFINED, Some(readable)),
    )?;
    readable.payload_mut().unwrap().controller.replace(rc);

    // Writable side
    let mut writable =
        JSWritableStream::new_instance(ctx, JSValue::UNDEFINED, WritableStreamPayload::new())?;
    let wc_payload = WritableStreamDefaultControllerPayload::new(transformer, Some(writable));
    let wcv = JSWritableStreamDefaultController::new_instance(ctx, JSValue::UNDEFINED, wc_payload)?;
    writable.payload_mut().unwrap().controller.replace(wcv);

    // Capture rc and writable again after mutable borrow
    let rc = readable.payload().unwrap().controller.unwrap();
    let mut wc = writable.payload().unwrap().controller.unwrap();
    let transformer = if let Some(cp) = wc.payload() {
        cp.underlying_sink
    } else {
        JSValue::UNDEFINED
    };

    // Transform controller
    let tc = JSTransformStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        TransformStreamDefaultControllerPayload::new(rc, writable, transformer),
    )?;

    // Wire the transform controller into the writable controller
    if let Some(wcp) = wc.payload_mut() {
        wcp.transform_controller = Some(tc);
    }

    // Create the TransformStream instance
    let instance = JSTransformStream::new_instance(
        ctx,
        new_target,
        TransformStreamPayload {
            readable: Some(readable),
            writable: Some(writable),
        },
    )?;

    // Call transformer.start(transformController) if present
    if let Some(obj) = transformer.as_object()
        && let Ok(start_fn) = obj.get_property_str(ctx, "start")
        && start_fn.is_callable()
    {
        // The transform controller is stored on the writable controller — retrieve it
        let tc = (|| -> Option<JSTransformStreamDefaultController> {
            let wcp = wc.payload()?;
            wcp.transform_controller
        })();
        if let Some(tc) = tc {
            let _ = start_fn.call(ctx, transformer, &[JSValue::from(tc)]);
        }
    }

    Ok(instance.into())
}

fn js_transform_stream_readable(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStream"))?;
    Ok(p.readable.map(JSValue::from).unwrap_or(JSValue::UNDEFINED))
}

fn js_transform_stream_writable(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStream"))?;
    Ok(p.writable.map(JSValue::from).unwrap_or(JSValue::UNDEFINED))
}

// ============================================================================
// TransformStreamDefaultController
// ============================================================================

pub(crate) struct TransformStreamDefaultControllerPayload {
    readable_controller: JSReadableStreamDefaultController,
    writable_stream: JSWritableStream,
    transformer: JSValue,
}

impl TransformStreamDefaultControllerPayload {
    fn new(
        readable_controller: JSReadableStreamDefaultController,
        writable_stream: JSWritableStream,
        transformer: JSValue,
    ) -> Self {
        Self {
            readable_controller,
            writable_stream,
            transformer,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.readable_controller.trace(gcx);
        self.writable_stream.trace(gcx);
        self.transformer.trace(gcx);
    }
}

const TRANSFORM_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("enqueue", 1, js_transform_controller_enqueue, 0),
    FieldDef::FUNC("error", 1, js_transform_controller_error, 0),
    FieldDef::FUNC("terminate", 0, js_transform_controller_terminate, 0),
    FieldDef::GETSET(
        "desiredSize",
        Some(js_transform_controller_desired_size),
        None,
        0,
    ),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "TransformStreamDefaultController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_transform_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("TransformStreamDefaultController cannot be constructed directly"))
}

def_native_object!(
    "TransformStreamDefaultController",
    JSTransformStreamDefaultController,
    TransformStreamDefaultControllerPayload,
    TRANSFORM_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_transform_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: TransformStreamDefaultControllerPayload::gc_trace
);

fn js_transform_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let p = JSTransformStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStreamDefaultController"))?;

    let rc = p.readable_controller;
    let rcp = rc
        .payload()
        .ok_or_else(|| ctx.throw_internal_error("invalid readable controller"))?;

    if rcp.close_requested.get() {
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
        rcp.queue.borrow_mut().push(QueueEntry { chunk, size: 1.0 });
        let total = rcp.queue_total_size.get();
        rcp.queue_total_size.set(total + 1.0);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_transform_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let reason = cc.arg(0);
    let p = JSTransformStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStreamDefaultController"))?;

    // Error the readable side via the readable controller
    if let Some(rcp) = p.readable_controller.payload() {
        PENDING_READ.with(|slot| {
            let mut s = slot.borrow_mut();
            if let Some((mut promise, _slot)) = s.take() {
                promise.reject(ctx, reason);
            }
        });

        if let Some(mut stream_val) = rcp.stream
            && let Some(sp) = stream_val.payload_mut()
        {
            sp.state = RsState::Errored;
            sp.stored_error = reason;
        }
    }

    // Error the writable side
    if let Some(wsp) = p.writable_stream.payload() {
        // Can't mutate through shared ref; error via the writable controller
        if let Some(wc) = wsp.controller
            && let Some(wcp) = wc.payload()
            && let Some(mut ws) = wcp.stream
            && let Some(wsp_mut) = ws.payload_mut()
        {
            wsp_mut.state = RsState::Errored;
            wsp_mut.stored_error = reason;
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_transform_controller_terminate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStreamDefaultController"))?;

    // Close the readable side
    if let Some(rcp) = p.readable_controller.payload() {
        rcp.close_requested.set(true);

        PENDING_READ.with(|slot| {
            let mut s = slot.borrow_mut();
            if let Some((_promise, rr)) = s.take()
                && let Ok(result) = make_iterator_result(ctx, JSValue::UNDEFINED, true)
            {
                let _ = rr[0].call(ctx, JSValue::UNDEFINED, &[result]);
            }
        });

        if rcp.queue.borrow().is_empty()
            && let Some(mut stream_val) = rcp.stream
            && let Some(sp) = stream_val.payload_mut()
        {
            sp.state = RsState::Closed;
        }
    }

    // Error the writable side
    let err = ctx
        .create_string_from_utf8(b"TransformStream terminated".to_vec())
        .map(JSValue::make_string)
        .unwrap_or(JSValue::UNDEFINED);
    if let Some(wsp) = p.writable_stream.payload()
        && let Some(wc) = wsp.controller
        && let Some(wcp) = wc.payload()
        && let Some(mut ws) = wcp.stream
        && let Some(wsp_mut) = ws.payload_mut()
    {
        wsp_mut.state = RsState::Errored;
        wsp_mut.stored_error = err;
    }

    Ok(JSValue::UNDEFINED)
}

fn js_transform_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStreamDefaultController"))?;

    if let Some(rcp) = p.readable_controller.payload() {
        let total = rcp.queue_total_size.get();
        return Ok(JSValue::from(1.0 - total));
    }
    Ok(JSValue::from(0.0))
}

// ============================================================================
// QueuingStrategy payload
// ============================================================================

pub(crate) struct QueuingStrategyPayload;

// ============================================================================
// ByteLengthQueuingStrategy
// ============================================================================

const BYTE_LENGTH_QUEUING_STRATEGY_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("size", 1, js_byte_length_size, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ByteLengthQueuingStrategy",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "ByteLengthQueuingStrategy",
    JSByteLengthQueuingStrategy,
    QueuingStrategyPayload,
    BYTE_LENGTH_QUEUING_STRATEGY_PROTO,
    Some(ClassCtorDef::new(js_byte_length_constructor, 1, false).expose_to_global())
);

fn js_byte_length_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let init_val = cc.arg(0);
    let mut instance =
        JSByteLengthQueuingStrategy::new_instance(ctx, new_target, QueuingStrategyPayload)?;
    if let Some(obj) = init_val.as_object()
        && let Ok(hwm) = obj.get_property_str(ctx, "highWaterMark")
    {
        instance.define_property_value_str(ctx, "highWaterMark", hwm, zuqe::JS_PROP_C_W_E)?;
    }
    Ok(instance.into())
}

// ============================================================================
// CountQueuingStrategy
// ============================================================================

const COUNT_QUEUING_STRATEGY_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("size", 1, js_count_size, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "CountQueuingStrategy",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "CountQueuingStrategy",
    JSCountQueuingStrategy,
    QueuingStrategyPayload,
    COUNT_QUEUING_STRATEGY_PROTO,
    Some(ClassCtorDef::new(js_count_constructor, 1, false).expose_to_global())
);

fn js_count_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let init_val = cc.arg(0);
    let mut instance =
        JSCountQueuingStrategy::new_instance(ctx, new_target, QueuingStrategyPayload)?;
    if let Some(obj) = init_val.as_object()
        && let Ok(hwm) = obj.get_property_str(ctx, "highWaterMark")
    {
        instance.define_property_value_str(ctx, "highWaterMark", hwm, zuqe::JS_PROP_C_W_E)?;
    }
    Ok(instance.into())
}

fn js_byte_length_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.arg(0);
    if let Some(s) = chunk.as_string(false) {
        return Ok(JSValue::from(s.len() as f64));
    }
    if let Ok(arr) = TypedArray::try_from(chunk) {
        return Ok(JSValue::from(arr.byte_length() as i64));
    }

    let len = chunk
        .get_property_str(ctx, "byteLength")
        .or_else(|_| chunk.get_property_str(ctx, "length"))
        .unwrap_or(JSValue::from(0.0));

    Ok(JSValue::from(
        len.to_number(ctx).unwrap_or(zuqe::IntOrFloat::Int64(0)),
    ))
}

fn js_count_size(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Ok(JSValue::from(1.0))
}

// ============================================================================
// Helpers
// ============================================================================

fn make_iterator_result(
    ctx: &mut ScopedContext,
    value: JSValue,
    done: bool,
) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;
    obj.define_property_value_str(ctx, "value", value, zuqe::JS_PROP_C_W_E)?;
    obj.define_property_value_str(ctx, "done", JSValue::from(done), zuqe::JS_PROP_C_W_E)?;
    Ok(JSValue::from(obj))
}

fn make_resolved_promise(ctx: &mut ScopedContext, value: JSValue) -> Result<JSValue, i32> {
    let (promise, rr) = Promise::create(ctx)?;
    rr[0].call(ctx, JSValue::UNDEFINED, &[value])?;
    Ok(promise.into())
}

fn make_rejected_promise(ctx: &mut ScopedContext, reason: JSValue) -> Result<JSValue, i32> {
    let (promise, rr) = Promise::create(ctx)?;
    rr[1].call(ctx, JSValue::UNDEFINED, &[reason])?;
    Ok(promise.into())
}

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

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStream::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream_default_writer(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStreamDefaultWriter::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStreamDefaultController::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStream::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStreamDefaultController::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_byte_length_queuing_strategy(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSByteLengthQueuingStrategy::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_count_queuing_strategy(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSCountQueuingStrategy::register(rt)
}
