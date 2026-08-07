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
    Array, CallContext, Callback, ClassCtorDef, ClassID, ErrorKind, Exception, FieldDef, JSResult,
    JSValue, JobGcTrace, JobState, Object, Runtime, ScopedContext,
    builtins::{
        iterator::{AsyncIterableIterator, AsyncIteratorFetcher, IterFetchStatus},
        promise::{Promise, PromiseResultSlot, PromiseState},
        typed_array::TypedArray,
    },
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    http::client::body::BodySource,
    whatwg::{
        abort::{JSAbortSignal, signal_is_aborted, signal_reason},
        dom::make_dom_exception,
        stream::{extract_high_water_mark, writable::JSWritableStream},
    },
};

use super::{QueueEntry, RsState, make_iterator_result, settle_stream_state};

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

impl JSReadableStream {
    /// Lock this stream by acquiring a reader. Creates a default (or BYOB)
    /// reader bound to this stream and stores it in the stream's `reader`
    /// field. Returns the reader as a JSValue. Errors with TypeError if the
    /// stream is already locked.
    ///
    /// Shared by `getReader()` and the internal "lock the body so a later
    /// getReader() throws" operations (e.g. after text()/json()/bytes()
    /// consumed the body), so the lock semantics live in exactly one place.
    pub(crate) fn acquire_reader(
        &mut self,
        ctx: &mut ScopedContext,
        byob: bool,
    ) -> Result<JSReadableStreamDefaultReader, i32> {
        let handle = *self;

        let sp = self
            .payload_mut()
            .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;

        if sp.reader.is_some() {
            return Err(ctx.throw_type_error("ReadableStream is already locked"));
        }

        let mut closed_promise = Promise::create(ctx)?.0;
        match sp.state {
            RsState::Closed => closed_promise.resolve(ctx, JSValue::UNDEFINED),
            RsState::Errored => closed_promise.reject(ctx, sp.stored_error),
            RsState::Readable => {}
        }
        let reader = JSReadableStreamDefaultReader::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultReaderPayload {
                stream: Some(handle),
                closed_promise: Some(closed_promise),
                byob,
            },
        )?;
        sp.reader = Some(reader);

        Ok(reader)
    }

    /// Synchronously read this stream's body into a byte buffer. Valid only
    /// for a stream whose data is already queued (e.g. a constructor stream
    /// populated in `start`); a pending read cannot be waited on synchronously.
    /// Locks the stream via `acquire_reader` so a later `getReader()` throws.
    pub(crate) fn drain_sync(&self, ctx: &mut ScopedContext) -> Result<Vec<u8>, i32> {
        let mut this = *self;

        let reader_obj = this.acquire_reader(ctx, false)?;
        let p = reader_obj
            .payload()
            .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;

        let mut buf = Vec::new();

        loop {
            let pv = p.read(ctx, None)?;
            let pr = Promise::try_from(pv)
                .map_err(|_| ctx.throw_internal_error("read() returned a non-promise"))?;
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

const READABLE_STREAM_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("locked", Some(js_readable_stream_locked), None, 0),
    FieldDef::GETSET("state", Some(js_readable_stream_state), None, 0),
    FieldDef::FUNC("getReader", 0, js_readable_stream_get_reader, 0),
    FieldDef::FUNC("cancel", 1, js_readable_stream_cancel, 0),
    FieldDef::FUNC("pipeTo", 1, js_readable_stream_pipe_to, 0),
    FieldDef::FUNC("pipeThrough", 1, js_readable_stream_pipe_through, 0),
    FieldDef::FUNC("tee", 0, js_readable_stream_tee, 0),
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
    let queuing_strategy = cc.arg(1);

    let payload = ReadableStreamPayload::new();
    let mut instance = JSReadableStream::new_instance(ctx, new_target, payload)?;

    // A byte stream uses a `ReadableByteStreamController`; a default stream uses
    // the `ReadableStreamDefaultController`. Both are created and the matching
    // one is passed to `start(controller)`.
    let is_bytes = {
        let ty = underlying_source
            .as_object()
            .and_then(|o| o.get_property_str(ctx, "type").ok())
            .unwrap_or(JSValue::UNDEFINED);
        !ty.is_undefined()
            && ty
                .to_string(ctx)
                .map(|s| s.to_string() == "bytes")
                .unwrap_or(false)
    };

    let default_controller =
        create_readable_stream_controller(ctx, &instance, &underlying_source, &queuing_strategy)?;
    instance.payload_mut().unwrap().controller = Some(default_controller);

    let start_controller: JSValue = if is_bytes {
        let hwm = extract_high_water_mark(ctx, queuing_strategy)?;
        let bc = JSReadableByteStreamController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableByteStreamControllerPayload::new(
                if underlying_source.is_undefined_or_null() {
                    JSValue::UNDEFINED
                } else {
                    underlying_source
                },
                Some(instance),
                hwm,
            ),
        )?;
        bc.into()
    } else {
        default_controller.into()
    };

    // Spec: a throwing underlying start() errors the stream's controller (the
    // constructor itself does not throw); reject any read against it.
    if let Some(o) = underlying_source.as_object()
        && let Some(Err(_)) = o.invoke_str_defined(ctx, "start", &[start_controller])?
    {
        let e = ctx.take_exception().to_value(ctx);
        start_controller.invoke_str_defined(ctx, "error", &[e])?;
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
    let this = cc.this();

    if JSReadableStream::payload_of(&this).is_none() {
        return Err(ctx.throw_type_error("not a ReadableStream"));
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

    let mut stream = JSReadableStream::try_from(this)?;
    let reader = stream.acquire_reader(ctx, byob)?;
    Ok(JSValue::from(reader))
}

fn js_readable_stream_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    readable_stream_cancel(ctx, &cc.this(), cc.arg(0))
}

// ============================================================================
// ReadableStream.from(asyncIterable)
// ============================================================================
// Builds a ReadableStream whose chunks are the yielded values of an async (or
// sync) iterable. Chunks are pulled **on demand**: the target controller's
// `pull` drives iterator.next() only when a read needs data (spec behavior),
// and each pull fetches at most one value. A resolved {done:false, value}
// enqueues `value`; {done:true} closes the stream; a rejection errors it.
//
// Callback `data` layout for the pull chain:
//   [0] = iterator object, [1] = iterator.next(), [2] = controller
fn js_readable_stream_from_static(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let source = cc.arg(0);
    // Spec: the source must be an Object. Primitives (incl. strings, which are
    // iterable) are rejected with a TypeError.
    if source.as_object().is_none() {
        return Err(ctx.throw_type_error("ReadableStream.from: source must be an object"));
    }
    let iterator = source
        .get_iterator(ctx, true)
        .map_err(|_| -1)?
        .ok_or_else(|| {
            ctx.throw_type_error(
                "value has neither [Symbol.asyncIterator] nor [Symbol.iterator] property",
            )
        })?;
    let next_fn = iterator.get_property_str(ctx, "next")?;
    if !next_fn.is_callable() {
        return Err(ctx.throw_type_error("ReadableStream.from: iterator has no callable next()"));
    }

    // Underlying source whose `pull` drives the iterator on demand.
    let pull_fn = ctx
        .create_native_function_with_data(
            Callback::Func(from_pump_step),
            "fromPull",
            1,
            0,
            [JSValue::from(iterator), next_fn].into(),
        )?
        .into();
    let mut source_obj = ctx.create_base_object()?;
    source_obj.define_property_value_str(ctx, "pull", pull_fn, zuqe::JS_PROP_C_W_E)?;
    let source_val = JSValue::from(source_obj);

    let mut stream =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
    let mut controller = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(source_val, Some(stream), 1.0),
    )?;
    // Wire the controller's self-handle (call_controller_pull needs it).
    let handle = controller;
    controller
        .payload_mut()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
        .controller = Some(handle);
    stream.payload_mut().unwrap().controller = Some(controller);
    Ok(JSValue::from(stream))
}

// Controller `pull` entry: fetch at most one value from the iterator and enqueue
// it. Continues to the next pull only while reads are still waiting (driven via
// queue_job), so next() is never called past what readers need.
fn from_pump_step(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let iterator = Object::try_from(cc.data(0)).map_err(|_| -1)?;
    let next_fn = cc.data(1);
    let controller = cc.arg(0);
    from_pull_next(ctx, iterator, next_fn, controller)
}

// Call iterator.next(), await the result, and branch into
// from_pump_fulfilled / from_pump_rejected.
fn from_pull_next(
    ctx: &mut ScopedContext,
    iterator: Object,
    next_fn: JSValue,
    controller: JSValue,
) -> JSResult {
    let next_promise = next_fn.call(ctx, JSValue::from(iterator), &[])?;
    // Spec: next()'s result is wrapped via PromiseResolve. If it is already a
    // promise use it; otherwise (next() returned a bare value) resolve it so the
    // iterator-result-object check below runs and errors the stream if needed.
    let mut promise: Promise = match Promise::try_from(next_promise) {
        Ok(p) => p,
        Err(_) => {
            let (mut p, _rr) = Promise::create(ctx)?;
            p.resolve(ctx, next_promise);
            p
        }
    };
    let step = ctx
        .create_native_function_with_data(
            Callback::Func(from_pump_fulfilled),
            "fromPump",
            1,
            0,
            [JSValue::from(iterator), next_fn, controller].into(),
        )?
        .into();
    let fail = ctx
        .create_native_function_with_data(
            Callback::Func(from_pump_rejected),
            "fromPumpErr",
            1,
            0,
            [JSValue::from(iterator), next_fn, controller].into(),
        )?
        .into();
    promise.then(ctx, step, fail)?;
    Ok(JSValue::UNDEFINED)
}

// Await iterator.next() -> {done, value}. Enqueue `value`, then keep pulling
// only if reads are still outstanding; close when done.
fn from_pump_fulfilled(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let _iterator = Object::try_from(cc.data(0)).map_err(|_| -1)?;
    let _next_fn = cc.data(1);
    let controller = cc.data(2);
    let result = cc.arg(0);
    // Spec: iterator.next() must fulfill with an iterator result object; a
    // non-object (e.g. a bare number) errors the stream with a TypeError.
    if result.as_object().is_none() {
        let _ = ctx.throw_type_error("iterator result must be an object");
        let e = match ctx.take_exception() {
            zuqe::Exception::Error(v) => v,
            _ => JSValue::UNDEFINED,
        };
        if !tee_branch_is_final(ctx, controller) {
            let error_fn = controller.get_property_str(ctx, "error")?;
            error_fn.call(ctx, controller, &[e])?;
        }
        return Ok(JSValue::UNDEFINED);
    }
    let done = result
        .get_property_str(ctx, "done")?
        .to_bool()
        .unwrap_or(false);
    if done {
        // Skip close() if the controller already errored/closed; otherwise
        // close() errors propagate via `?` (the promise-reaction job takes and
        // rejects with any thrown exception).
        if !tee_branch_is_final(ctx, controller) {
            let close_fn = controller.get_property_str(ctx, "close")?;
            close_fn.call(ctx, controller, &[])?;
        }
        return Ok(JSValue::UNDEFINED);
    }
    let value = result.get_property_str(ctx, "value")?;
    if !tee_branch_is_final(ctx, controller) {
        let enqueue_fn = controller.get_property_str(ctx, "enqueue")?;
        enqueue_fn.call(ctx, controller, &[value])?;
    }
    // Pull the next value only if a read is still waiting (avoid over-pulling).
    let more = JSReadableStreamDefaultController::payload_of(&controller)
        .map(|cp| !cp.pending_read.borrow().is_empty())
        .unwrap_or(false);
    if more {
        let ctrl = controller;
        ctx.queue_job(
            move |cx| {
                let p = JSReadableStreamDefaultController::payload_of(&ctrl)
                    .ok_or_else(|| cx.throw_internal_error("invalid controller"))?;
                p.call_controller_pull(cx)?;
                Ok(JobState::Done)
            },
            (),
        );
    }
    Ok(JSValue::UNDEFINED)
}

// Await iterator.next() rejected: error the stream with the rejection reason.
// Guard so error() is not called on an already-terminated controller.
fn from_pump_rejected(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let controller = cc.data(2);
    // Skip error() if the controller already errored; otherwise error() errors
    // propagate via `?` (the promise-reaction job takes and rejects with it).
    if !tee_branch_is_final(ctx, controller) {
        let error_fn = controller.get_property_str(ctx, "error")?;
        error_fn.call(ctx, controller, &[cc.arg(0)])?;
    }
    Ok(JSValue::UNDEFINED)
}

// ============================================================================
// ReadableStream.prototype.tee()
// ============================================================================
// Returns [branch1, branch2], two ReadableStreams that both receive every chunk
// of `this`. The source is locked with a default reader, and a read-ahead pump
// pulls chunks and enqueues them into both branches; source error propagates to
// both; source close closes both.
//
// Callback `data` layout for the tee pump:
//   [0] = source reader, [1] = reader.read(), [2] = branch1 controller,
//   [3] = branch2 controller
fn js_readable_stream_tee(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    if JSReadableStream::payload_of(&this).is_none() {
        return Err(ctx.throw_type_error("tee: this must be a ReadableStream"));
    }
    if JSReadableStream::payload_of(&this)
        .unwrap()
        .reader
        .is_some()
    {
        return Err(ctx.throw_type_error("ReadableStream is already locked"));
    }
    let reader = this
        .get_property_str(ctx, "getReader")?
        .call(ctx, this, &[])?;
    // Consume the source reader's `closed` rejection (created by getReader) so an
    // errored source does not leak an unhandled rejection from this internal reader.
    if let Ok(closed) = reader.get_property_str(ctx, "closed")
        && let Ok(mut closed_p) = Promise::try_from(closed)
    {
        let noop = ctx
            .create_native_function(zuqe::Callback::Func(tee_ignore), "teeIgnore", 0, 0)?
            .into();
        closed_p.then(ctx, noop, noop)?;
    }

    // Each branch's underlying source `pull` is the shared on-demand tee pull,
    // keyed by a tee id that resolves to the shared state (source reader + both
    // branch controllers). Pull only fires when a branch actually needs data, so
    // an infinite source is never drained unless a branch asks for it.
    let tee_id = NEXT_TEE_ID.with(|c| {
        let id = c.get();
        c.set(id + 1);
        id
    });

    // Create two branch streams, each with its own default controller whose
    // underlying-source pull drives the shared tee pull.
    let make_branch = |ctx: &mut ScopedContext| -> Result<(JSValue, JSValue), i32> {
        let pull_fn = ctx
            .create_native_function_with_data(
                Callback::Func(tee_pull),
                "teePull",
                1,
                0,
                [JSValue::from(tee_id as f64)].into(),
            )?
            .into();
        let mut source = ctx.create_base_object()?;
        source.define_property_value_str(ctx, "pull", pull_fn, zuqe::JS_PROP_C_W_E)?;
        let mut stream =
            JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
        let mut controller = JSReadableStreamDefaultController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream), 1.0),
        )?;
        // Wire the controller's self-handle (call_controller_pull needs it).
        let handle = controller;
        controller
            .payload_mut()
            .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
            .controller = Some(handle);
        stream.payload_mut().unwrap().controller = Some(controller);
        Ok((JSValue::from(stream), JSValue::from(controller)))
    };
    let (branch1_val, c1) = make_branch(ctx)?;
    let (branch2_val, c2) = make_branch(ctx)?;

    let shared = Rc::new(TeeShared {
        reader,
        c1,
        c2,
        pulling: Cell::new(false),
        done: Cell::new(false),
    });
    TEE_STREAMS.with(|map| map.borrow_mut().insert(tee_id, shared));

    // The branches are exposed in a new array.
    let mut arr: JSValue = Array::with_capacity(ctx, 2)?.into();
    arr.set_property_i64(ctx, 0, branch1_val)?;
    arr.set_property_i64(ctx, 1, branch2_val)?;
    Ok(arr)
}

// On-demand tee pull: driven by a branch controller's `pull` (fired only when a
// branch read needs data). Fetches at most one source chunk and enqueues it into
// both live branches. A shared `pulling` guard prevents concurrent pulls from
// both branches; a shared `done` stops pulling after the source finishes.
fn tee_pull(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_f64() as u64;
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };
    tee_do_pull(ctx, tee_id, &shared)
}

// Source read -> {done, value}: enqueue into both live branches and clear the
// pulling guard. Close both when the source is done.
fn tee_pull_fulfilled(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_f64() as u64;
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };
    shared.pulling.set(false);
    let result = cc.arg(0);
    let done = result
        .get_property_str(ctx, "done")?
        .to_bool()
        .unwrap_or(false);
    if done {
        shared.done.set(true);
        for c in [shared.c1, shared.c2] {
            if !tee_branch_is_final(ctx, c) {
                let close_fn = c.get_property_str(ctx, "close")?;
                close_fn.call(ctx, c, &[])?;
            }
        }
        TEE_STREAMS.with(|m| m.borrow_mut().remove(&tee_id));
        return Ok(JSValue::UNDEFINED);
    }
    let value = result.get_property_str(ctx, "value")?;
    for c in [shared.c1, shared.c2] {
        if !tee_branch_is_final(ctx, c) {
            let enqueue_fn = c.get_property_str(ctx, "enqueue")?;
            enqueue_fn.call(ctx, c, &[value])?;
        }
    }
    // If any branch still has an outstanding read waiting for data, keep pulling
    // (concurrent reads on a branch produce multiple pending reads; one source
    // chunk only satisfies one of them).
    let need_more = [shared.c1, shared.c2]
        .iter()
        .any(|&c| !tee_branch_is_final(ctx, c) && tee_has_pending_read(ctx, c));
    if need_more {
        let id = tee_id;
        ctx.queue_job(
            move |cx| {
                if let Some(shared) = TEE_STREAMS.with(|m| m.borrow().get(&id).cloned()) {
                    tee_do_pull(cx, id, &shared)?;
                }
                Ok(JobState::Done)
            },
            (),
        );
    }
    Ok(JSValue::UNDEFINED)
}

// True when the branch controller has an outstanding pending read waiting for
// a chunk to be enqueued.
fn tee_has_pending_read(_ctx: &mut ScopedContext, c: JSValue) -> bool {
    let Some(cp) = JSReadableStreamDefaultController::payload_of(&c) else {
        return false;
    };
    !cp.pending_read.borrow().is_empty()
}

// Perform the shared pull: read one source chunk and fan it out to both branches.
fn tee_do_pull(ctx: &mut ScopedContext, tee_id: u64, shared: &TeeShared) -> JSResult {
    if shared.pulling.get() || shared.done.get() {
        return Ok(JSValue::UNDEFINED);
    }
    shared.pulling.set(true);
    let p = JSReadableStreamDefaultReader::payload_of(&shared.reader)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;

    let mut promise = p.read(ctx, None)?;

    let step = ctx
        .create_native_function_with_data(
            Callback::Func(tee_pull_fulfilled),
            "teePullOk",
            1,
            0,
            [JSValue::from(tee_id as f64)].into(),
        )?
        .into();
    let fail = ctx
        .create_native_function_with_data(
            Callback::Func(tee_pull_rejected),
            "teePullErr",
            1,
            0,
            [JSValue::from(tee_id as f64)].into(),
        )?
        .into();
    promise.then(ctx, step, fail)?;

    Ok(JSValue::UNDEFINED)
}

// Source read rejected: error both branches and clear the pulling guard.
fn tee_pull_rejected(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_f64() as u64;
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };
    shared.pulling.set(false);
    shared.done.set(true);
    let reason = cc.arg(0);
    for c in [shared.c1, shared.c2] {
        if !tee_branch_is_final(ctx, c) {
            let error_fn = c.get_property_str(ctx, "error")?;
            error_fn.call(ctx, c, &[reason])?;
        }
    }
    TEE_STREAMS.with(|m| m.borrow_mut().remove(&tee_id));
    Ok(JSValue::UNDEFINED)
}

// A branch controller is "final" (errored or closed): skip enqueue/close/error.
fn tee_branch_is_final(_ctx: &mut ScopedContext, c: JSValue) -> bool {
    let Some(cp) = JSReadableStreamDefaultController::payload_of(&c) else {
        return true;
    };
    let Some(s) = cp.stream else {
        return true;
    };
    let Some(sp) = s.payload() else {
        return true;
    };
    matches!(sp.state, RsState::Errored | RsState::Closed)
}

// No-op handler used to consume the internal source reader's `closed` rejection.
fn tee_ignore(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Ok(JSValue::UNDEFINED)
}

// Pipe machinery: pipeTo locks the source synchronously (disturbing it per
// WHATWG spec) and copies data to `dest` asynchronously. The WPT tests only
// require that locking/disturbing happens synchronously; the transfer runs as
// a microtask chain via reader.read()/writer.write() promises.
//
// Callback `data` layout shared across the async driver:
//   [0] = result resolve fn, [1] = result reject fn, [2] = reader,
//   [3] = writer, [4] = signal (UNDEFINED if none)

fn js_readable_stream_pipe_to(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    // Spec: pipeTo always returns a promise; brand/validation failures reject it
    // (never throw synchronously), so `promise_rejects_js` gets a promise.
    let bad_this = JSReadableStream::payload_of(&this).is_none();
    let dest = cc.arg(0);
    let bad_dest = JSWritableStream::payload_of(&dest).is_none();
    let options = cc.arg(1);
    // Extract and validate `signal`: it must be undefined or an AbortSignal.
    let signal = if options.is_undefined_or_null() {
        JSValue::UNDEFINED
    } else if let Some(obj) = options.as_object() {
        obj.get_property_str(ctx, "signal")?
    } else {
        JSValue::UNDEFINED
    };
    let bad_signal = !signal.is_undefined() && JSAbortSignal::payload_of(&signal).is_none();

    if bad_this || bad_dest || bad_signal {
        let msg = if bad_this {
            "pipeTo: this must be a ReadableStream"
        } else if bad_dest {
            "pipeTo: dest must be a WritableStream"
        } else {
            "options.signal must be an AbortSignal"
        };
        let _ = ctx.throw_type_error(msg);
        let e = match ctx.take_exception() {
            zuqe::Exception::Error(v) => v,
            _ => JSValue::UNDEFINED,
        };
        return Promise::from_reject_value(ctx, e).map(JSValue::from);
    }
    let writer = dest
        .get_property_str(ctx, "getWriter")?
        .call(ctx, dest, &[])?;
    // Acquire a reader on the source synchronously: this locks (disturbs) the
    // stream immediately so `locked` is true and a later getReader() throws.
    // `this` is already a ReadableStream (checked above): acquire its reader via
    // the Rust-side API instead of the JS-layer `getReader`.
    let mut src_stream = JSReadableStream::try_from(this)?;
    let reader = src_stream.acquire_reader(ctx, false)?;

    run_pipe_driver(ctx, reader, writer, signal, options)
}

// Shared pipe driver: drives `reader` -> `writer` microtask-by-microtask.
// Called by both pipeTo (after brand checks + signal validation) and pipeThrough
// (with duck-typed transform's reader/writer). `signal` is undefined or a valid
// AbortSignal. `options` drives the abort reaction flags.
fn run_pipe_driver(
    ctx: &mut ScopedContext,
    reader: JSReadableStreamDefaultReader,
    writer: JSValue,
    signal: JSValue,
    options: JSValue,
) -> JSResult {
    let (promise, rr) = Promise::create(ctx)?;
    let data = [rr[0], rr[1], JSValue::from(reader), writer, signal, options];

    // Synchronous abort check: a signal already aborted must reject the pipe
    // promise without doing any reads.
    if !signal.is_undefined() && crate::whatwg::abort::signal_is_aborted(&signal) {
        pipe_to_perform_abort(ctx, reader, writer, signal, options, rr[1])?;
        release_pipe_locks(ctx, JSValue::from(reader), writer);
        return Ok(JSValue::from(promise));
    }

    // Register an abort listener when a live signal is present.
    if !signal.is_undefined() && signal.as_object().is_some() {
        let abort_handler = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_to_on_abort),
                "pipeToOnAbort",
                1,
                0,
                data.into(),
            )?
            .into();
        let abort_name = JSValue::make_string(ctx.create_string_from_utf8(b"abort".to_vec())?);
        let add_listener = signal.get_property_str(ctx, "addEventListener")?;

        // The signal is already brand-checked by pipeTo, so addEventListener
        // won't throw here; if it somehow does, ignore it rather than turn the
        // pipeTo call into a synchronous throw (it must return a promise).
        if add_listener
            .call(ctx, signal, &[abort_name, abort_handler])
            .is_err()
        {
            let _e = ctx.take_exception();
            #[cfg(debug_assertions)]
            eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
        }
    }

    let data = [rr[0], rr[1], JSValue::from(reader), writer, signal];
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

    let p = reader
        .payload()
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;
    let mut read_promise = p.read(ctx, None)?;
    read_promise.then(ctx, step, step_fail)?;

    Ok(JSValue::from(promise))
}

// Release the reader/writer locks acquired by pipeTo/pipeThrough once the pipe
// ends. Spec: pipeTo releases both locks when it settles, so a later
// getReader()/getWriter() on the same streams succeeds. A releaseLock() that
// throws (e.g. the reader is already released) must not abort the pipe: drop
// the exception (take_exception) so it doesn't pollute cur_exception.
fn release_pipe_locks(ctx: &mut ScopedContext, reader: JSValue, writer: JSValue) {
    for target in [reader, writer] {
        if let Ok(rel) = target.get_property_str(ctx, "releaseLock")
            && rel.call(ctx, target, &[]).is_err()
        {
            let _e = ctx.take_exception();
            #[cfg(debug_assertions)]
            eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
        }
    }
}

// Abort signal fired mid-pipe: abort the writer, cancel the reader, reject the
// pipe promise with the signal's reason, and release the locks.
fn pipe_to_on_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reject = cc.data(1);
    let reader = JSReadableStreamDefaultReader::try_from(cc.data(2))?;
    let writer = cc.data(3);
    let signal = cc.data(4);
    let options = cc.data(5);
    pipe_to_perform_abort(ctx, reader, writer, signal, options, reject)?;
    release_pipe_locks(ctx, JSValue::from(reader), writer);
    Ok(JSValue::UNDEFINED)
}

// Perform the abort-triggered shutdown of a pipe: abort the destination writer
// and cancel the source reader, then reject the pipe promise with the most
// appropriate error. Per the streams spec, preference order is:
//   1. the error from `writer.abort()` (sink),
//   2. the error from `reader.cancel()` (source),
//   3. the signal's abort reason.
// Both cleanup calls use the same `reason` object so the writer's `closed` and
// the reader's `closed` reject with the same object the pipe promise rejects
// with (WPT: "all the error objects should be the same object").
fn pipe_to_perform_abort(
    ctx: &mut ScopedContext,
    reader: JSReadableStreamDefaultReader,
    writer: JSValue,
    signal: JSValue,
    options: JSValue,
    reject: JSValue,
) -> JSResult {
    let reason = if !signal.is_undefined() {
        crate::whatwg::abort::signal_reason(&signal).unwrap_or(JSValue::UNDEFINED)
    } else {
        JSValue::UNDEFINED
    };
    let (prevent_cancel, prevent_abort) = if let Some(obj) = options.as_object() {
        let pc = obj
            .get_property_str(ctx, "preventCancel")?
            .to_bool()
            .unwrap_or(false);
        let pa = obj
            .get_property_str(ctx, "preventAbort")?
            .to_bool()
            .unwrap_or(false);
        (pc, pa)
    } else {
        (false, false)
    };

    // Collect the cleanup actions that must run.
    //
    // NOTE: `actions` intentionally holds only an enum tag, never the JSValue
    // targets. In the arena GC model, native-stack-local JSValues are no longer
    // traced (only values pushed into a `f.call` arena frame are), so storing
    // raw JSValues in a heap `Vec` would leave them with no GC root and risk a
    // use-after-free if a nested JS call triggers GC. The target objects
    // (`writer`/`reader`) live in this closure's binding data, which is traced
    // via `NFuncPayload::trace`, so we resolve them from those rooted locals
    // inside the loop instead of caching the values here.
    #[derive(Clone, Copy)]
    enum ShutdownAction {
        Abort,
        Cancel,
    }
    let mut actions: Vec<ShutdownAction> = Vec::new();
    if !prevent_abort {
        actions.push(ShutdownAction::Abort);
    }
    if !prevent_cancel {
        actions.push(ShutdownAction::Cancel);
    }

    // No actions (both prevented, or streams already settled): reject with the
    // signal reason directly.
    if actions.is_empty() {
        reject.call(ctx, JSValue::UNDEFINED, &[reason])?;
        return Ok(JSValue::UNDEFINED);
    }

    // Shared aggregation state across the action settle callbacks.
    let mut state = ctx.create_base_object()?;
    state.set_property_str(ctx, "pending", (actions.len() as f64).into())?;
    state.set_property_str(ctx, "abortErr", JSValue::UNDEFINED)?;
    state.set_property_str(ctx, "cancelErr", JSValue::UNDEFINED)?;

    for action in actions {
        // Resolve the target from the GC-rooted locals (`writer`/`reader`)
        // rather than from a cached JSValue, keeping the value under a root.
        let (method, target): (&'static str, JSValue) = match action {
            ShutdownAction::Abort => ("abort", writer),
            ShutdownAction::Cancel => ("cancel", JSValue::from(reader)),
        };
        let mut action_promise = if let Ok(f) = target.get_property_str(ctx, method) {
            match f.call(ctx, target, &[reason]) {
                Ok(res) => {
                    if let Ok(p) = Promise::try_from(res) {
                        p
                    } else {
                        Promise::from_resolve_value(ctx, res)?
                    }
                }
                Err(_) => {
                    let _e = ctx.take_exception();
                    Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?
                }
            }
        } else {
            let _e = ctx.take_exception();
            Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?
        };

        // Field on `state` that records this action's rejection reason.
        let field = if method == "abort" {
            "abortErr"
        } else {
            "cancelErr"
        };
        let field_val =
            JSValue::make_string(ctx.create_string_from_utf8(field.as_bytes().to_vec())?);
        let ok_data = [
            JSValue::from(state),
            JSValue::from(reader),
            writer,
            reject,
            reason,
        ];
        let err_data = [
            JSValue::from(state),
            field_val,
            JSValue::from(reader),
            writer,
            reject,
            reason,
        ];
        let ok_fn = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_shutdown_ok),
                "pipeShutdownOk",
                0,
                0,
                ok_data.into(),
            )?
            .into();
        let err_fn = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_shutdown_err),
                "pipeShutdownErr",
                1,
                0,
                err_data.into(),
            )?
            .into();
        action_promise.then(ctx, ok_fn, err_fn)?;
    }

    Ok(JSValue::UNDEFINED)
}

// Action fulfilled: record nothing, decrement the pending counter, finalize.
fn pipe_shutdown_ok(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let state = cc.data(0);
    let reader = cc.data(1);
    let writer = cc.data(2);
    let reject = cc.data(3);
    let reason = cc.data(4);
    pipe_shutdown_finalize(ctx, state, reader, writer, reject, reason)
}

// Action rejected: record the rejection reason for this action (in the field
// named by `field`), decrement the pending counter, then finalize.
fn pipe_shutdown_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let state = cc.data(0);
    let field_val = cc.data(1);
    let reader = cc.data(2);
    let writer = cc.data(3);
    let reject = cc.data(4);
    let reason = cc.data(5);
    let err = cc.arg(0);
    let field_str = field_val.to_string(ctx)?;
    let field = field_str.content().encode_utf8(false);
    let mut state_obj = state.as_object().unwrap();
    state_obj.set_property_str(ctx, &field, err)?;
    pipe_shutdown_finalize(ctx, state, reader, writer, reject, reason)
}

// Once every action has settled, reject the pipe promise with the most
// appropriate error: sink abort error > source cancel error > signal reason.
fn pipe_shutdown_finalize(
    ctx: &mut ScopedContext,
    state: JSValue,
    reader: JSValue,
    writer: JSValue,
    reject: JSValue,
    reason: JSValue,
) -> JSResult {
    let mut state_obj = state.as_object().unwrap();
    let pending = state_obj
        .get_property_str(ctx, "pending")?
        .to_number(ctx)?
        .to_f64();
    state_obj.set_property_str(ctx, "pending", (pending - 1.0).into())?;
    if (pending - 1.0).abs() < f64::EPSILON {
        let abort_err = state_obj.get_property_str(ctx, "abortErr")?;
        let cancel_err = state_obj.get_property_str(ctx, "cancelErr")?;
        let final_err = if !abort_err.is_undefined() {
            abort_err
        } else if !cancel_err.is_undefined() {
            cancel_err
        } else {
            reason
        };
        reject.call(ctx, JSValue::UNDEFINED, &[final_err])?;
        release_pipe_locks(ctx, reader, writer);
    }
    Ok(JSValue::UNDEFINED)
}

fn js_readable_stream_pipe_through(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    let mut this_stream = JSReadableStream::try_from(this).map_err(|e| {
        let _ = ctx.throw_type_error("pipeThrough: this must be a ReadableStream");
        e
    })?;

    let transform = cc.arg(0);
    // `transform` is duck-typed: `{ readable, writable }`. Brand-check `readable`
    // first (so a bad readable does not access `writable`), then `writable`.
    let readable = transform.get_property_str(ctx, "readable")?;
    if JSReadableStream::payload_of(&readable).is_none() {
        return Err(
            ctx.throw_type_error("pipeThrough: transform.readable must be a ReadableStream")
        );
    }
    let writable = transform.get_property_str(ctx, "writable")?;
    if JSWritableStream::payload_of(&writable).is_none() {
        return Err(
            ctx.throw_type_error("pipeThrough: transform.writable must be a WritableStream")
        );
    }

    let options = cc.arg(1);
    // Extract and validate `signal`: it must be undefined or an AbortSignal.
    let signal = if options.is_undefined_or_null() {
        JSValue::UNDEFINED
    } else if let Some(obj) = options.as_object() {
        obj.get_property_str(ctx, "signal")?
    } else {
        JSValue::UNDEFINED
    };
    if !signal.is_undefined() && JSAbortSignal::payload_of(&signal).is_none() {
        return Err(ctx.throw_type_error("options.signal must be an AbortSignal"));
    }

    // pipeThrough = pipe this into transform.writable, then return transform.readable.
    // Use the shared driver directly (NOT this.pipeTo) so a monkey-patched
    // pipeTo on the prototype/instance is not invoked.
    let writer = writable
        .get_property_str(ctx, "getWriter")?
        .call(ctx, writable, &[])?;

    let reader = this_stream.acquire_reader(ctx, false)?;
    run_pipe_driver(ctx, reader, writer, signal, options)?;

    Ok(readable)
}

// Read one chunk and schedule the next step.
fn pipe_to_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reader = cc.data(2);
    let writer = cc.data(3);
    let signal = cc.data(4);
    let p = JSReadableStreamDefaultReader::payload_of(&reader)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;
    let mut read_promise = p.read(ctx, None)?;

    let step = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_read),
            "pipeToOnRead",
            1,
            0,
            [cc.data(0), cc.data(1), reader, writer, signal].into(),
        )?
        .into();
    let step_fail = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_read_error),
            "pipeToOnReadErr",
            1,
            0,
            [cc.data(0), cc.data(1), reader, writer, signal].into(),
        )?
        .into();
    read_promise.then(ctx, step, step_fail)?;
    Ok(JSValue::UNDEFINED)
}

// A chunk was read: close on done, otherwise write and loop.
fn pipe_to_on_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let result = cc.arg(0);
    let done = result.get_property_str(ctx, "done")?.to_bool()?;
    if done {
        let resolve = cc.data(0);
        let reject = cc.data(1);
        let reader = cc.data(2);
        let writer = cc.data(3);
        // Close the destination; resolve the pipe promise when done. A close
        // that throws synchronously (e.g. destination already errored) must
        // not crash the driver — reject the pipe promise instead.
        let close_fn = writer.get_property_str(ctx, "close")?;
        let close_js = match close_fn.call(ctx, writer, &[]) {
            Ok(v) => v,
            Err(_) => {
                // close() threw synchronously: take the exception (it is set
                // even though the call returned Err) and reject the pipe
                // promise with it as the reason, instead of crashing the driver.
                let err = match ctx.take_exception() {
                    zuqe::Exception::Error(v) => v,
                    _ => JSValue::UNDEFINED,
                };
                reject.call(ctx, JSValue::UNDEFINED, &[err])?;
                release_pipe_locks(ctx, reader, writer);
                return Ok(JSValue::UNDEFINED);
            }
        };
        let mut close_promise: Promise = Promise::try_from(close_js)?;
        let settle_ok = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_to_settle_ok),
                "pipeToSettleOk",
                0,
                0,
                [resolve, JSValue::UNDEFINED, reader, writer].into(),
            )?
            .into();
        let settle_err = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_to_settle_err),
                "pipeToSettleErr",
                1,
                0,
                [resolve, reject, reader, writer].into(),
            )?
            .into();
        close_promise.then(ctx, settle_ok, settle_err)?;
        return Ok(JSValue::UNDEFINED);
    }
    let value = result.get_property_str(ctx, "value")?;
    let writer = cc.data(3);
    let signal = cc.data(4);
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
            [cc.data(0), cc.data(1), cc.data(2), writer, signal].into(),
        )?
        .into();
    let write_fail = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_write_error),
            "pipeToWriteErr",
            1,
            0,
            [cc.data(0), cc.data(1), cc.data(2), writer, signal].into(),
        )?
        .into();
    write_promise.then(ctx, loop_fn, write_fail)?;
    Ok(JSValue::UNDEFINED)
}

// Source read failed: abort the writer, reject the pipe promise, release locks.
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
    release_pipe_locks(ctx, reader, writer);
    Ok(JSValue::UNDEFINED)
}

// Write failed: abort the writer, cancel the reader, reject, release locks.
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
    release_pipe_locks(ctx, reader, writer);
    Ok(JSValue::UNDEFINED)
}

// Settle the pipe promise once the destination close finishes.
// Success path (close fulfilled): resolve the pipe promise and release locks.
fn pipe_to_settle_ok(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let resolve = cc.data(0);
    release_pipe_locks(ctx, cc.data(2), cc.data(3));
    resolve.call(ctx, JSValue::UNDEFINED, &[])?;
    Ok(JSValue::UNDEFINED)
}

// Failure path (close rejected): reject the pipe promise with the close error
// and release locks.
fn pipe_to_settle_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reject = cc.data(1);
    let reason = cc.arg(0);
    release_pipe_locks(ctx, cc.data(2), cc.data(3));
    reject.call(ctx, JSValue::UNDEFINED, &[reason])?;
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

/// Shared state for a `tee()` split: one source reader feeding two branch
/// controllers on demand. `pulling` guards against concurrent pulls from both
/// branches; `done` stops pulling once the source finishes.
struct TeeShared {
    reader: JSValue,
    c1: JSValue,
    c2: JSValue,
    pulling: Cell<bool>,
    done: Cell<bool>,
}

thread_local! {
    pub(super) static TEE_STREAMS: RefCell<HashMap<u64, Rc<TeeShared>>> = RefCell::new(HashMap::new());
    static NEXT_TEE_ID: Cell<u64> = const { Cell::new(1) };
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

            if !cp.queue.borrow().is_empty() {
                let mut q = cp.queue.borrow_mut();
                let entry = q.remove(0);
                let total = cp.queue_total_size.get();
                cp.queue_total_size.set(total - entry.size);

                // BYOB: copy as much of the chunk as fits into the caller's view
                // and push the remainder back so the next read continues where
                // this one stopped. Done before the close transition so a
                // partially consumed chunk keeps the stream readable.
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
                return Promise::from_resolve_value(ctx, result);
            }

            if cp.close_requested.get() {
                let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
                return Promise::from_resolve_value(ctx, result);
            }

            // No data in queue — register a pending read on this controller so a
            // later enqueue()/close()/error() can resolve it. Stored per-controller
            // (not globally) so concurrent/sequential streams don't steal chunks.
            let (promise, rr) = Promise::create(ctx)?;
            cp.pending_read.borrow_mut().push([rr[0], rr[1]]);
            cp.call_controller_pull(ctx)?;
            return Ok(promise.into());
        }

        // No controller (e.g. detached) — nothing to read.
        let result = make_iterator_result(ctx, JSValue::UNDEFINED, true)?;
        Promise::from_resolve_value(ctx, result)
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
    pub(crate) high_water_mark: f64,
    pub(crate) started: Cell<bool>,
    pub(crate) close_requested: Cell<bool>,
    pub(crate) pull_again: Cell<bool>,
    pub(crate) pulling: Cell<bool>,
    /// Queue of in-flight read() resolvers awaiting data (each is [resolve,
    /// reject]). Kept as a queue so concurrent read() calls each get their own
    /// resolver instead of clobbering the previous one. Stored per-controller
    /// (not globally) so concurrent/sequential streams do not steal chunks.
    pub(crate) pending_read: Rc<RefCell<Vec<[JSValue; 2]>>>,
    /// Self JS handle, set right after instance creation, used to re-queue
    /// pull jobs.
    pub(crate) controller: Option<JSReadableStreamDefaultController>,
}

impl ReadableStreamDefaultControllerPayload {
    pub(crate) fn new(
        underlying_source: JSValue,
        stream: Option<JSReadableStream>,
        high_water_mark: f64,
    ) -> Self {
        Self {
            stream,
            underlying_source,
            queue: Rc::new(RefCell::new(Vec::new())),
            queue_total_size: Rc::new(Cell::new(0.0)),
            high_water_mark,
            started: Cell::new(false),
            close_requested: Cell::new(false),
            pull_again: Cell::new(false),
            pulling: Cell::new(false),
            pending_read: Rc::new(RefCell::new(Vec::new())),
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
        for rr in self.pending_read.borrow().iter() {
            rr[0].trace(gcx);
            rr[1].trace(gcx);
        }
    }

    /// Resolve the controller's pending read (if any) with an iterator result
    /// `{value, done}`. Returns true if a pending read was present and resolved.
    /// Resolve the controller's *oldest* pending read (if any) with an iterator
    /// result `{value, done}`. Returns true if a pending read was resolved.
    pub(super) fn resolve_pending_read(
        &self,
        ctx: &mut ScopedContext,
        value: JSValue,
        done: bool,
    ) -> bool {
        let rr = self.pending_read.borrow_mut().first().copied();
        if let Some([resolve, _reject]) = rr {
            if let Ok(result) = make_iterator_result(ctx, value, done) {
                self.pending_read.borrow_mut().remove(0);
                if resolve.call(ctx, JSValue::UNDEFINED, &[result]).is_err() {
                    let _e = ctx.take_exception();
                    #[cfg(debug_assertions)]
                    eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
                }
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Reject the controller's *oldest* pending read with `reason`.
    pub(super) fn reject_pending_read(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let rr = self.pending_read.borrow_mut().first().copied();
        if let Some([_resolve, reject]) = rr {
            self.pending_read.borrow_mut().remove(0);
            // fn returns (); a throwing resolver is abnormal, drop its exception.
            if reject.call(ctx, JSValue::UNDEFINED, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
        }
    }

    /// Reject every outstanding pending read with `reason` (used on error).
    pub(super) fn reject_all_pending_reads(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let reads = self.pending_read.borrow_mut().drain(..).collect::<Vec<_>>();
        for [_, reject] in reads {
            if reject.call(ctx, JSValue::UNDEFINED, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
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
        // Spec: if the underlying source's pull() throws, error the stream with
        // that value (which rejects any pending reads) rather than swallowing it.
        let pull_res = pull_fn.call(ctx, *source, &[JSValue::from(controller_val)]);
        if pull_res.is_err() {
            let err = match ctx.take_exception() {
                zuqe::Exception::Error(v) => v,
                _ => JSValue::UNDEFINED,
            };
            self.pulling.set(false);
            self.reject_all_pending_reads(ctx, err);
            self.queue.borrow_mut().clear();
            self.queue_total_size.set(0.0);
            if let Some(stream_val) = self.stream {
                settle_stream_state(ctx, stream_val.into(), RsState::Errored, Some(err));
            }
            return Ok(());
        }

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

    // Spec: enqueue must throw when the stream is already closed or errored.
    if let Some(stream_val) = p.stream {
        if let Some(sp) = stream_val.payload() {
            if sp.state == RsState::Errored {
                return Err(ctx.throw_type_error("cannot enqueue to an errored stream"));
            }
            if sp.state == RsState::Closed {
                return Err(ctx.throw_type_error("cannot enqueue to a closed stream"));
            }
        }
    }

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

    // Spec: close must throw when the stream is already closed or errored.
    if let Some(stream_val) = p.stream
        && let Some(sp) = stream_val.payload()
    {
        if sp.state == RsState::Errored {
            return Err(ctx.throw_type_error("cannot close an errored stream"));
        }
        if sp.state == RsState::Closed {
            return Err(ctx.throw_type_error("cannot close a closed stream"));
        }
    }

    p.close_requested.set(true);

    // Every outstanding read() past the queue now resolves as done.
    while p.resolve_pending_read(ctx, JSValue::UNDEFINED, true) {}

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

    // Spec: error must throw when the stream is already closed or errored.
    if let Some(stream_val) = p.stream {
        if let Some(sp) = stream_val.payload() {
            if sp.state == RsState::Errored {
                return Err(ctx.throw_type_error("cannot error an errored stream"));
            }
            if sp.state == RsState::Closed {
                return Err(ctx.throw_type_error("cannot error a closed stream"));
            }
        }
    }

    // Reject every outstanding read() with the error, and drop any queued
    // chunks (spec: an errored stream discards its queue, so later reads
    // reject instead of draining stale data).
    p.reject_all_pending_reads(ctx, e);
    p.queue.borrow_mut().clear();
    p.queue_total_size.set(0.0);

    if let Some(stream_val) = p.stream {
        settle_stream_state(ctx, stream_val.into(), RsState::Errored, Some(e));
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;
    // Spec: errored -> null, closed -> 0, else highWaterMark - queueTotalSize.
    if let Some(s) = p.stream
        && let Some(sp) = s.payload()
    {
        match sp.state {
            RsState::Errored => return Ok(JSValue::NULL),
            RsState::Closed => return Ok(JSValue::from(0.0)),
            RsState::Readable => {}
        }
    }
    let total = p.queue_total_size.get();
    Ok(JSValue::from(p.high_water_mark - total))
}

// ============================================================================
// Controller setup helpers
// ============================================================================

fn create_readable_stream_controller(
    ctx: &mut ScopedContext,
    stream: &JSReadableStream,
    underlying_source: &JSValue,
    queuing_strategy: &JSValue,
) -> Result<JSReadableStreamDefaultController, i32> {
    let hwm = extract_high_water_mark(ctx, *queuing_strategy)?;
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
            hwm,
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

fn readable_stream_cancel(
    ctx: &mut ScopedContext,
    stream: &JSValue,
    reason: JSValue,
) -> Result<JSValue, i32> {
    if let Some(mut stream_obj) = stream.as_native_object::<JSReadableStream>() {
        // Spec: cancel() sets disturbed, so the stream is no longer usable as
        // a body even though no read() ever happened.
        if let Some(p) = stream_obj.payload_mut() {
            p.disturbed = true;
        }

        // Spec: reader.cancel() rejects the reader's `closed` promise with
        // `reason` (which may be undefined for a bare cancel). pipeTo's abort
        // path relies on this to make reader.closed reject with the abort reason
        // so all error objects are the same object. We model cancel as an
        // Errored settlement (not Closed) so a later getReader().closed also
        // rejects with `reason` per spec.
        settle_stream_state(ctx, (*stream_obj).into(), RsState::Errored, Some(reason));

        if let Some(p) = stream_obj.payload()
            && let Some(controller_val) = p.controller
            && let Some(cp) = controller_val.payload()
        {
            let source = cp.underlying_source;
            // Propagate the underlying source's cancel() result (including a
            // rejection) to the reader.cancel() promise — pipeTo relies on this
            // to surface source cancel errors.
            return if let Some(o) = source.as_object() {
                o.invoke_str_defined(ctx, "cancel", &[reason])?
                    .unwrap_or(Ok(JSValue::UNDEFINED))
            } else {
                Ok(JSValue::UNDEFINED)
            };
        }
    }

    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
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
            let p = match JSReadableStreamDefaultReader::payload_of(&reader_val) {
                Some(p) => p,
                None => return Ok(IterFetchStatus::Done),
            };

            let promise = p.read(ctx, None)?;
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
// Channel/bytes/text-backed stream constructors
// ============================================================================

impl JSReadableStream {
    /// Create a `ReadableStream` backed by a `crossbeam::Receiver<Result<Vec<u8>, String>>`.
    ///
    /// Uses a background microtask (queue_job) to poll the channel and push
    /// chunks into the stream's controller. A `Ok(bytes)` item enqueues a chunk;
    /// an `Err(msg)` item errors the stream (the reader's `read()` rejects); when
    /// the channel is disconnected the stream is closed.
    pub(crate) fn from_channel(
        ctx: &mut ScopedContext,
        rx: Receiver<Result<Vec<u8>, String>>,
        signal: Option<JSValue>,
    ) -> Result<Self, i32> {
        let source = ctx.create_base_object()?;

        let mut stream =
            JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;

        let controller = JSReadableStreamDefaultController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream), 1.0),
        )?;
        stream.payload_mut().unwrap().controller = Some(controller);

        let id = NEXT_STREAM_ID.with(|n| {
            let cur = n.get();
            n.set(cur + 1);
            cur
        });

        // Keep the abort signal alive on the stream payload (GC-traced), not just in
        // the channel registry, so it cannot be reclaimed while the body is live.
        let sp = stream.payload_mut().unwrap();
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

        Ok(stream)
    }

    /// Create a `ReadableStream` from a single byte buffer (synthetic body).
    ///
    /// Unlike `from_channel`, this does not create or poll a
    /// channel — the data is enqueued and the stream is closed immediately.
    pub(crate) fn from_bytes(ctx: &mut ScopedContext, bytes: Vec<u8>) -> Result<Self, i32> {
        let source = ctx.create_base_object()?;

        let mut stream =
            JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;

        let controller = JSReadableStreamDefaultController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream), 1.0),
        )?;
        let controller_val = JSValue::from(controller);

        stream.payload_mut().unwrap().controller = Some(controller);

        // Enqueue the single chunk as a TypedArray.
        let ta = TypedArray::create_from_vec_u8(ctx, bytes)?;
        let enqueue_fn = controller_val.get_property_str(ctx, "enqueue")?;
        enqueue_fn.call(ctx, controller_val, &[JSValue::from(ta)])?;

        // Close the stream — data is the full body.
        let close_fn = controller_val.get_property_str(ctx, "close")?;
        close_fn.call(ctx, controller_val, &[])?;

        Ok(stream)
    }

    /// Create a `ReadableStream` from an already-decoded text string. Non-empty
    /// text is enqueued as a single string chunk; empty text produces a closed,
    /// chunk-free stream. Used by Request/Response `textStream()`.
    pub(crate) fn from_text(ctx: &mut ScopedContext, text: String) -> Result<Self, i32> {
        let source = ctx.create_base_object()?;
        let mut stream =
            JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
        let controller = JSReadableStreamDefaultController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream), 1.0),
        )?;
        let controller_val = JSValue::from(controller);
        stream.payload_mut().unwrap().controller = Some(controller);

        if !text.is_empty() {
            let sval = ctx.create_string_from_utf8(text.into_bytes())?;
            let enqueue_fn = controller_val.get_property_str(ctx, "enqueue")?;
            enqueue_fn.call(ctx, controller_val, &[JSValue::make_string(sval)])?;
        }
        // Close the stream — the text is the entire body.
        let close_fn = controller_val.get_property_str(ctx, "close")?;
        close_fn.call(ctx, controller_val, &[])?;

        Ok(stream)
    }

    /// Create a `ReadableStream` of UTF-8-decoded string chunks from a byte
    /// channel. Each `Ok(bytes)` item is decoded and enqueued as a string; an
    /// `Err(msg)` item errors the stream; disconnect closes it. Used by
    /// `textStream()` on network responses.
    pub(crate) fn from_text_channel(
        ctx: &mut ScopedContext,
        rx: Receiver<Result<Vec<u8>, String>>,
    ) -> Result<Self, i32> {
        let source = ctx.create_base_object()?;
        let mut stream =
            JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
        let controller = JSReadableStreamDefaultController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream), 1.0),
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
                    enqueue_fn.call(cx, JSValue::from(ctrl), &[JSValue::make_string(sval)])?;
                    Ok(JobState::Again)
                }
                Ok(Err(msg)) => {
                    let err = cx
                        .create_error_object(ErrorKind::Type, &msg, false)
                        .ok_or(-1)?;
                    let error_fn = ctrl.get_property_str(cx, "error")?;
                    error_fn.call(cx, JSValue::from(ctrl), &[err.into()])?;
                    Ok(JobState::Done)
                }
                Err(TryRecvError::Disconnected) => {
                    let close_fn = ctrl.get_property_str(cx, "close")?;
                    close_fn.call(cx, JSValue::from(ctrl), &[])?;
                    Ok(JobState::Done)
                }
                Err(TryRecvError::Empty) => {
                    if start.elapsed() > TEXT_STREAM_IDLE_TIMEOUT {
                        let err = cx.create_string_from_utf8(b"text stream timeout".to_vec())?;
                        let error_fn = ctrl.get_property_str(cx, "error")?;
                        error_fn.call(cx, JSValue::from(ctrl), &[JSValue::make_string(err)])?;
                        Ok(JobState::Done)
                    } else {
                        Ok(JobState::Again)
                    }
                }
            },
            (),
        );

        Ok(stream)
    }

    /// Build a `ReadableStream` of UTF-8-decoded string chunks from a body source.
    /// Used by Request/Response `textStream()`. A `Stream` body is drained
    /// synchronously, so it must already be populated (as in the WPT tests); a
    /// live `Channel` is decoded incrementally by a polling job.
    pub(crate) fn from_text_source(
        ctx: &mut ScopedContext,
        source: BodySource,
    ) -> Result<Self, i32> {
        match source {
            BodySource::Synthetic(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Self::from_text(ctx, text)
            }
            BodySource::Stream(stream) => {
                let bytes = stream.drain_sync(ctx)?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Self::from_text(ctx, text)
            }
            BodySource::Channel(rx) => Self::from_text_channel(ctx, rx),
        }
    }
}

fn start_channel_stream_poller(ctx: &mut ScopedContext) {
    // Idle timeout, applied per stream and refreshed on every received chunk.
    // A stream is only killed when its own producer goes quiet for this long,
    // so neither a slow body nor a long-lived sibling stream can starve it.
    const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

    ctx.queue_job(
        move |cx| {
            CHANNEL_STREAMS.with(|map| -> Result<(), i32> {
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
                            let ef = controller.get_property_str(cx, "error")?;
                            ef.call(cx, JSValue::from(controller), &[reason])?;
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
                            let ta = TypedArray::create_from_vec_u8(cx, data).map_err(|_| {
                                cx.throw_internal_error("TypedArray creation failed")
                            })?;
                            let enqueue_fn = controller.get_property_str(cx, "enqueue")?;
                            enqueue_fn.call(cx, JSValue::from(controller), &[JSValue::from(ta)])?;
                        }
                        Some(Ok(Err(msg))) => {
                            // Underlying network stream failed mid-body: error.
                            let bytes = msg.into_bytes();
                            let err = cx
                                .create_error_object(
                                    ErrorKind::Type,
                                    &String::from_utf8_lossy(&bytes),
                                    false,
                                )
                                .ok_or(-1)?;
                            let error_fn = controller.get_property_str(cx, "error")?;
                            error_fn.call(cx, JSValue::from(controller), &[err.into()])?;
                            to_remove.push(id);
                        }
                        Some(Err(TryRecvError::Disconnected)) => {
                            let close_fn = controller.get_property_str(cx, "close")?;
                            close_fn.call(cx, JSValue::from(controller), &[])?;
                            to_remove.push(id);
                        }
                        Some(Err(TryRecvError::Empty)) => {
                            let expired = map
                                .borrow()
                                .get(&id)
                                .map(|(_, _, d)| d.elapsed() > STREAM_IDLE_TIMEOUT)
                                .unwrap_or(true);
                            if expired {
                                let err = cx
                                    .create_string_from_utf8(b"channel stream timeout".to_vec())
                                    .map(JSValue::make_string)
                                    .map_err(|_| -1)?;
                                let error_fn = controller.get_property_str(cx, "error")?;
                                error_fn.call(cx, JSValue::from(controller), &[err])?;
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
                Ok(())
            })?;

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
// ReadableByteStreamController (minimal exposure)
//
// The full byte-stream engine (byte queue, pull-into, BYOB request lifecycle)
// is not implemented. This class exists so that `ReadableByteStreamController`
// is a visible global constructor and so that `new ReadableStream({ type:
// "bytes" }, strategy)` passes a `ReadableByteStreamController` instance to
// the user's `start(controller)` callback (per WPT construct-byob-request).
// Methods reuse the shared stream-state transitions; end-to-end byte transfer
// through `read()` is intentionally not wired up yet.
// ============================================================================

pub(crate) struct ReadableByteStreamControllerPayload {
    pub(crate) stream: Option<JSReadableStream>,
    pub(crate) underlying_source: JSValue,
    pub(crate) queue: Rc<RefCell<Vec<QueueEntry>>>,
    pub(crate) queue_total_size: Rc<Cell<f64>>,
    pub(crate) high_water_mark: f64,
    pub(crate) close_requested: Cell<bool>,
    /// Pending BYOB request view, if any. Left `None` in this minimal build.
    pub(crate) byob_request: Rc<RefCell<Option<JSValue>>>,
}

impl ReadableByteStreamControllerPayload {
    pub(crate) fn new(
        underlying_source: JSValue,
        stream: Option<JSReadableStream>,
        high_water_mark: f64,
    ) -> Self {
        Self {
            stream,
            underlying_source,
            queue: Rc::new(RefCell::new(Vec::new())),
            queue_total_size: Rc::new(Cell::new(0.0)),
            high_water_mark,
            close_requested: Cell::new(false),
            byob_request: Rc::new(RefCell::new(None)),
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_source.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref br) = *self.byob_request.borrow() {
            br.trace(gcx);
        }
    }
}

const BYTE_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("enqueue", 1, js_byte_controller_enqueue, 0),
    FieldDef::FUNC("close", 0, js_byte_controller_close, 0),
    FieldDef::FUNC("error", 1, js_byte_controller_error, 0),
    FieldDef::GETSET(
        "desiredSize",
        Some(js_byte_controller_desired_size),
        None,
        0,
    ),
    FieldDef::GETSET(
        "byobRequest",
        Some(js_byte_controller_byob_request),
        None,
        0,
    ),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableByteStreamController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_byte_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("ReadableByteStreamController cannot be constructed directly"))
}

fn js_byte_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    // Spec: errored -> null, closed -> 0, else highWaterMark - queueTotalSize.
    if let Some(s) = p.stream
        && let Some(sp) = s.payload()
    {
        match sp.state {
            RsState::Errored => return Ok(JSValue::NULL),
            RsState::Closed => return Ok(JSValue::from(0.0)),
            RsState::Readable => {}
        }
    }
    let total = p.queue_total_size.get();
    Ok(JSValue::from(p.high_water_mark - total))
}

fn js_byte_controller_byob_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    // Minimal build: no pending BYOB request is tracked.
    Ok(p.byob_request.borrow().unwrap_or(JSValue::UNDEFINED))
}

fn js_byte_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;

    if p.close_requested.get() {
        return Err(ctx.throw_type_error("cannot enqueue after close"));
    }

    // Minimal build: enqueue the chunk with unit size. Full byte-stream size
    // accounting (byte length) is not wired up yet.
    p.queue.borrow_mut().push(QueueEntry { chunk, size: 1.0 });
    p.queue_total_size.set(p.queue_total_size.get() + 1.0);

    Ok(JSValue::UNDEFINED)
}

fn js_byte_controller_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;

    p.close_requested.set(true);

    if p.queue.borrow().is_empty() {
        if let Some(stream_val) = p.stream {
            settle_stream_state(ctx, stream_val.into(), RsState::Closed, None);
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_byte_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;

    if let Some(stream_val) = p.stream {
        settle_stream_state(ctx, stream_val.into(), RsState::Errored, Some(e));
    }

    Ok(JSValue::UNDEFINED)
}

def_native_object!(
    "ReadableByteStreamController",
    JSReadableByteStreamController,
    ReadableByteStreamControllerPayload,
    BYTE_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_byte_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableByteStreamControllerPayload::gc_trace
);

// ============================================================================
// Registration
// ============================================================================

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStream::register(rt)
}

/// Register the static `ReadableStream.from()` method on the global constructor.
pub(crate) fn install_readable_stream_statics(
    ctx: &mut ScopedContext,
    global: &mut Object,
) -> Result<(), i32> {
    let from_fn = ctx.create_native_function(
        zuqe::Callback::Func(js_readable_stream_from_static),
        "from",
        1,
        0,
    )?;
    let ctor_val = global.get_property_str(ctx, "ReadableStream")?;
    if let Some(mut ctor) = ctor_val.as_object() {
        ctor.define_property_value_str(ctx, "from", from_fn.into(), zuqe::JS_PROP_C_W_E)?;
    }
    Ok(())
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
fn register_readable_byte_stream_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableByteStreamController::register(rt)
}
