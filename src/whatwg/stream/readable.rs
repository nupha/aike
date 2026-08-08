// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, TryRecvError};
use linkme::distributed_slice;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, ErrorKind, Exception, FieldDef, JSResult,
    JSValue, JobState, Object, Runtime, ScopedContext,
    builtins::{
        iterator::{AsyncIterableIterator, AsyncIteratorFetcher, IterFetchStatus},
        promise::Promise,
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
        stream::{
            extract_high_water_mark,
            reader::{JSReadableStreamDefaultReader, ReaderPayload},
            writable::JSWritableStream,
        },
    },
};

use super::{
    JSReadableByteStreamController, JSReadableStreamDefaultController, ReadableControllerPayload,
    RsState, notify_peer_closed,
};

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

    /// Set this stream's state (and `stored_error` when errored), then settle
    /// the peer reader's `closed_promise` accordingly.
    ///
    /// A stream can only leave the Readable state once. Re-settling (e.g. cancel
    /// after an error) must not re-settle the peer's closed_promise, which would
    /// violate the Promise contract.
    pub(crate) fn settle_state(
        &mut self,
        ctx: &mut ScopedContext,
        state: RsState,
        reason: Option<JSValue>,
    ) {
        if self.state != RsState::Readable {
            return;
        }
        self.state = state;
        if let Some(r) = reason {
            self.stored_error = r;
        }
        notify_peer_closed(ctx, self.reader.map(JSValue::from), reason);
    }

    /// Cancel this stream. Sets `disturbed` and settles the stream as Closed
    /// (per spec `ReadableStreamCancel` -> `ReadableStreamClose`, so the peer
    /// reader's `closed` promise resolves), then propagates the underlying
    /// source's `cancel()` result (including a rejection) to the caller.
    pub(crate) fn cancel(
        &mut self,
        ctx: &mut ScopedContext,
        reason: JSValue,
    ) -> Result<Promise, i32> {
        // Spec: cancel() sets disturbed, so the stream is no longer usable as
        // a body even though no read() ever happened.
        self.disturbed = true;

        // Spec: cancel() closes the stream, not errors it — reader.closed must
        // resolve, and a later getReader().closed resolves too. Only a stream
        // error rejects `closed`. The cancel reason is forwarded to the
        // underlying source's cancel() below, not stored as the stream error.
        self.settle_state(ctx, RsState::Closed, None);

        if let Some(controller_val) = self.controller
            && let Some(cp) = controller_val.payload()
        {
            let source = cp.underlying_source();
            // Propagate the underlying source's cancel() result (including a
            // rejection) to the reader.cancel() promise — pipeTo relies on this
            // to surface source cancel errors.
            return if let Some(o) = source.as_object() {
                match o.invoke_str_defined(ctx, "cancel", &[reason])? {
                    Some(Ok(v)) => {
                        if let Ok(p) = Promise::try_from(v) {
                            Ok(p)
                        } else {
                            Promise::from_resolve_value(ctx, v)
                        }
                    }
                    Some(Err(_)) => {
                        let e = ctx.take_exception().to_value(ctx);
                        Promise::from_reject_value(ctx, e)
                    }
                    None => Promise::from_resolve_value(ctx, JSValue::UNDEFINED),
                }
            } else {
                Promise::from_resolve_value(ctx, JSValue::UNDEFINED)
            };
        }

        Promise::from_resolve_value(ctx, JSValue::UNDEFINED)
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
            ReaderPayload {
                stream: Some(handle),
                closed_promise: Some(closed_promise),
                byob,
            },
        )?;
        sp.reader = Some(reader);

        Ok(reader)
    }
}

const READABLE_STREAM_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("locked", Some(js_readable_stream_locked), None, 0),
    FieldDef::GETSET("state", Some(js_readable_stream_state), None, 0),
    FieldDef::FUNC("getReader", 0, js_readable_stream_get_reader, 0),
    FieldDef::FUNC("cancel", 1, js_readable_stream_cancel, 0),
    FieldDef::FUNC("pipeTo", 1, js_readable_stream_pipe_to, 0),
    FieldDef::FUNC("pipeThrough", 1, js_readable_stream_pipe_through, 0),
    FieldDef::FUNC("tee", 0, super::readable_tee::js_readable_stream_tee, 0),
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

pub(crate) fn get_readable_stream_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a ReadableStreamPayload, i32> {
    JSReadableStream::payload_of(val).ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))
}

pub(crate) fn get_readable_stream_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut ReadableStreamPayload, i32> {
    JSReadableStream::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))
}

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
            ReadableControllerPayload::new_byte(
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
    let p = get_readable_stream_payload(ctx, &this)?;
    Ok(JSValue::from(p.reader.is_some()))
}

fn js_readable_stream_state(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_readable_stream_payload(ctx, &this)?;
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
    let mut stream = cc
        .this()
        .as_native_object::<JSReadableStream>()
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;

    // options.mode: undefined -> default reader, "byob" -> BYOB reader,
    // anything else -> TypeError (the spec converts mode to a fixed enum).
    let options = cc.arg(0);
    let byob = if options.is_undefined_or_null() {
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

    let reader = stream.acquire_reader(ctx, byob)?;
    Ok(JSValue::from(reader))
}

fn js_readable_stream_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_readable_stream_payload_mut(ctx, &mut this)?;
    p.cancel(ctx, cc.arg(0)).map(JSValue::from)
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
        ReadableControllerPayload::new_default(source_val, Some(stream), 1),
    )?;
    // Wire the controller's self-handle (call_controller_pull needs it).
    let handle = controller;
    controller
        .payload_mut()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
        .set_controller(Some(handle));
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
        let e = ctx.take_exception().to_value(ctx);
        if !super::readable_tee::tee_branch_is_final(ctx, controller) {
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
        if !super::readable_tee::tee_branch_is_final(ctx, controller) {
            let close_fn = controller.get_property_str(ctx, "close")?;
            close_fn.call(ctx, controller, &[])?;
        }
        return Ok(JSValue::UNDEFINED);
    }
    let value = result.get_property_str(ctx, "value")?;
    if !super::readable_tee::tee_branch_is_final(ctx, controller) {
        let enqueue_fn = controller.get_property_str(ctx, "enqueue")?;
        enqueue_fn.call(ctx, controller, &[value])?;
    }
    // Pull the next value only if a read is still waiting (avoid over-pulling).
    let more = JSReadableStreamDefaultController::payload_of(&controller)
        .map(|cp| !cp.pending_read().borrow().is_empty())
        .unwrap_or(false);
    if more {
        let ctrl = controller;
        ctx.queue_job(
            move |cx| {
                let p = JSReadableStreamDefaultController::payload_of(&ctrl)
                    .ok_or_else(|| cx.throw_internal_error("invalid controller"))?;
                p.call_pull(cx)?;
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
    if !super::readable_tee::tee_branch_is_final(ctx, controller) {
        let error_fn = controller.get_property_str(ctx, "error")?;
        error_fn.call(ctx, controller, &[cc.arg(0)])?;
    }
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
        let e = ctx.take_exception().to_value(ctx);
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
//
// Coordination: all state that async pipe callbacks need (resolve/reject fns,
// reader/writer/signal/options, and the `settled` / `shutting_down` / abort
// aggregation flags) lives on a JS `state` object threaded through the callback
// `data` arrays. JS objects and the `data` arrays are both traced by the GC, so
// the JSValues they hold stay rooted for the transfer's lifetime.
//
// `state` fields:
//   settled: bool          - pipe promise settled exactly once (prevents the
//                            normal error path and the abort path from both
//                            rejecting the same promise).
//   shuttingDown: bool     - shutdown began; the read/write loop stops pulling
//                            and stops closing the destination.
//   remainingActions: int  - abort/error cleanup actions still settling.
//   abortErr, cancelErr    - first error from the abort/cancel cleanup action.
//   fallbackReason         - rejection reason when cleanup settles cleanly.
fn run_pipe_driver(
    ctx: &mut ScopedContext,
    reader: JSReadableStreamDefaultReader,
    writer: JSValue,
    signal: JSValue,
    options: JSValue,
) -> JSResult {
    let (promise, rr) = Promise::create(ctx)?;

    let mut state = ctx.create_base_object()?;
    state.set_property_str(ctx, "settled", false.into())?;
    state.set_property_str(ctx, "shuttingDown", false.into())?;
    state.set_property_str(ctx, "remainingActions", 0.into())?;
    state.set_property_str(ctx, "abortErr", JSValue::UNDEFINED)?;
    state.set_property_str(ctx, "cancelErr", JSValue::UNDEFINED)?;
    state.set_property_str(ctx, "fallbackReason", JSValue::UNDEFINED)?;

    // Uniform data layout threaded through every async pipe callback:
    //   [0] state, [1] resolve, [2] reject, [3] reader, [4] writer,
    //   [5] signal, [6] options
    let data = [
        state.into(),
        rr[0],
        rr[1],
        JSValue::from(reader),
        writer,
        signal,
        options,
    ];

    // Synchronous abort check: a signal already aborted must reject the pipe
    // promise without doing any reads.
    if !signal.is_undefined() && crate::whatwg::abort::signal_is_aborted(&signal) {
        pipe_to_perform_abort(ctx, &data)?;
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
        //
        // NOTE: if the signal was aborted between the check above and here,
        // addEventListener fires the handler synchronously; its Err is swallowed
        // by addEventListener (which returns Ok), so clean cur_exception
        // unconditionally rather than only on is_err().
        add_listener.call(ctx, signal, &[abort_name, abort_handler])?;
        if ctx.rt().exception() != Exception::No {
            ctx.take_exception();
        }
    }

    pipe_to_read(ctx, &data)?;
    Ok(JSValue::from(promise))
}

// True if the pipe has already settled (promise settled) or begun shutting down;
// used by the read/write loop and the abort handler to stop doing work.
fn pipe_is_shutting_down(ctx: &mut ScopedContext, state: &JSValue) -> Result<bool, i32> {
    let obj = state.as_object().unwrap();
    let settled = obj.get_property_str(ctx, "settled")?.to_bool()?;
    let shutting = obj.get_property_str(ctx, "shuttingDown")?.to_bool()?;
    Ok(settled || shutting)
}

// Mark the pipe as settled; returns true if this call claimed the settlement
// (i.e. it was not already settled). Guards all resolve/reject paths.
fn pipe_mark_settled(ctx: &mut ScopedContext, state: &JSValue) -> Result<bool, i32> {
    let mut obj = state.as_object().unwrap();
    if obj.get_property_str(ctx, "settled")?.to_bool()? {
        return Ok(false);
    }
    obj.set_property_str(ctx, "settled", true.into())?;
    Ok(true)
}

fn pipe_option_bool(ctx: &mut ScopedContext, options: &JSValue, key: &str) -> Result<bool, i32> {
    if let Some(obj) = options.as_object() {
        let b = obj.get_property_str(ctx, key)?.to_bool().unwrap_or(false);
        Ok(b)
    } else {
        Ok(false)
    }
}

// Kick off the read loop: read one chunk and chain the next step. Called both
// for the initial read and after each write resolves (loop continuation).
// `data` layout: [0] state, [1] resolve, [2] reject, [3] reader, [4] writer,
// [5] signal.
fn pipe_to_read(ctx: &mut ScopedContext, data: &[JSValue]) -> JSResult {
    // Once shutdown begins (abort or error), stop reading new data.
    if pipe_is_shutting_down(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    let reader = data[3];
    let p = JSReadableStreamDefaultReader::payload_of(&reader)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;
    let mut read_promise = p.read(ctx, None)?;

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
    read_promise.then(ctx, step, step_fail)?;
    Ok(JSValue::UNDEFINED)
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

// Abort signal fired mid-pipe: shut the pipe down with the abort reason. This
// is a no-op if the pipe has already settled or begun shutting down (e.g. the
// readable/writable already errored) — "abort should do nothing after the
// stream is closed/errored".
fn pipe_to_on_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    if pipe_is_shutting_down(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    set_state_bool(ctx, &data[0], "shuttingDown", true)?;
    pipe_to_perform_abort(ctx, &data)
}

fn set_state_bool(ctx: &mut ScopedContext, state: &JSValue, key: &str, v: bool) -> Result<(), i32> {
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, key, v.into())?;
    Ok(())
}

// Read the abort reason off the signal (or undefined if no signal).
fn pipe_signal_reason(_ctx: &mut ScopedContext, signal: &JSValue) -> JSValue {
    if !signal.is_undefined() {
        signal_reason(signal).unwrap_or(JSValue::UNDEFINED)
    } else {
        JSValue::UNDEFINED
    }
}

// Perform the abort-triggered shutdown of a pipe: abort the destination writer
// and cancel the source reader, then reject the pipe promise with the most
// appropriate error. Per the streams spec, preference order is:
//   1. the error from `writer.abort()` (sink),
//   2. the error from `reader.cancel()` (source), unless the source is already
//      in a final state (in which case cancel only returns its stale stored
//      error and must not override the abort reason),
//   3. the signal's abort reason.
// Both cleanup calls use the same `reason` object so the writer's `closed` and
// the reader's `closed` reject with the same object the pipe promise rejects
// with (WPT: "all the error objects should be the same object").
fn pipe_to_perform_abort(ctx: &mut ScopedContext, data: &[JSValue]) -> JSResult {
    let state = data[0];
    let reader = data[3];
    let options = data[6];
    let reason = pipe_signal_reason(ctx, &data[5]);
    let prevent_cancel = pipe_option_bool(ctx, &options, "preventCancel")?;
    let prevent_abort = pipe_option_bool(ctx, &options, "preventAbort")?;

    // The abort shutdown only waits for cleanup actions that must run. If both
    // are prevented (or both already moot), reject directly with the reason.
    let mut pending = 0;
    if !prevent_abort {
        pending += 1;
    }
    if !prevent_cancel && !pipe_source_is_final(&reader) {
        pending += 1;
    }
    if pending == 0 {
        return pipe_reject(ctx, data, reason);
    }

    // Aggregation state across the action settle callbacks: remaining actions
    // + the first error each action reports (sink abort error > source cancel
    // error) + the fallback reason (the abort reason).
    set_state_num(ctx, &state, "remainingActions", pending)?;
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, "abortErr", JSValue::UNDEFINED)?;
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, "cancelErr", JSValue::UNDEFINED)?;
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, "fallbackReason", reason)?;

    if !prevent_abort {
        pipe_run_shutdown_action(ctx, data, ShutdownAction::Abort, reason)?;
    }
    if !prevent_cancel && !pipe_source_is_final(&reader) {
        pipe_run_shutdown_action(ctx, data, ShutdownAction::Cancel, reason)?;
    }

    Ok(JSValue::UNDEFINED)
}

fn set_state_num(ctx: &mut ScopedContext, state: &JSValue, key: &str, n: u32) -> Result<(), i32> {
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, key, (n as f64).into())?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum ShutdownAction {
    Abort,
    Cancel,
}

// Run one abort cleanup action (`writer.abort(reason)` or `reader.cancel(reason)`)
// and register a settle callback that records its outcome and finalizes.
// `data` layout for the settle callback: [0] state, [1] resolve, [2] reject,
// [3] reader, [4] writer, [5] signal, [6] actionTag.
fn pipe_run_shutdown_action(
    ctx: &mut ScopedContext,
    data: &[JSValue],
    action: ShutdownAction,
    reason: JSValue,
) -> JSResult {
    let (method, target): (&'static str, JSValue) = match action {
        ShutdownAction::Abort => ("abort", data[4]),
        ShutdownAction::Cancel => ("cancel", data[3]),
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
                // abort()/cancel() threw synchronously: treat as a rejection
                // with the thrown error.
                let e = ctx.take_exception().to_value(ctx);
                Promise::from_reject_value(ctx, e)?
            }
        }
    } else {
        // Method missing: treat the action as a fulfilled no-op.
        Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?
    };

    let action_tag = if action == ShutdownAction::Abort {
        0.0
    } else {
        1.0
    };
    let settle_data = [
        data[0],
        data[1],
        data[2],
        data[3],
        data[4],
        data[5],
        JSValue::from(action_tag),
    ];
    let settle_fn = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_shutdown_action_settle),
            "pipeShutdownSettle",
            1,
            0,
            settle_data.into(),
        )?
        .into();
    action_promise.then(ctx, settle_fn, settle_fn)?;
    Ok(JSValue::UNDEFINED)
}

// One abort cleanup action settled. The single callback handles both the
// fulfilled (reason arg is undefined) and rejected (reason arg is the error)
// cases, recording the outcome and finalizing the abort shutdown.
// `data`: [0] state, [1] resolve, [2] reject, [3] reader, [4] writer,
// [5] signal, [6] actionTag.
fn pipe_shutdown_action_settle(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let state = cc.data(0);
    let is_abort = cc.data(6).to_number(ctx)?.to_f64() == 0.0;
    let arg = cc.arg(0);
    // `.then(settle, settle)` passes the fulfillment value on the fulfilled
    // path and the rejection reason on the rejected path.
    let err = if cc.length() > 0 && !arg.is_undefined() {
        Some(arg)
    } else {
        None
    };
    let mut state_obj = state.as_object().unwrap();
    if let Some(e) = err {
        let field = if is_abort { "abortErr" } else { "cancelErr" };
        state_obj.set_property_str(ctx, field, e)?;
    }

    let remaining = state_obj
        .get_property_str(ctx, "remainingActions")?
        .to_number(ctx)?
        .to_f64() as u32
        - 1;
    state_obj.set_property_str(ctx, "remainingActions", (remaining as f64).into())?;
    if remaining == 0 {
        let abort_err = state_obj.get_property_str(ctx, "abortErr")?;
        let cancel_err = state_obj.get_property_str(ctx, "cancelErr")?;
        let fallback = state_obj.get_property_str(ctx, "fallbackReason")?;
        let final_err = if !abort_err.is_undefined() {
            abort_err
        } else if !cancel_err.is_undefined() {
            cancel_err
        } else {
            fallback
        };
        let data = [
            cc.data(0),
            cc.data(1),
            cc.data(2),
            cc.data(3),
            cc.data(4),
            cc.data(5),
        ];
        return pipe_reject(ctx, &data, final_err);
    }
    Ok(JSValue::UNDEFINED)
}

// Settle the pipe promise with a rejection, guarded so it happens exactly once.
// Also releases the reader/writer locks. Returns the `reject` call result so
// the caller (a promise reaction job) can propagate it; swallowing it would
// leak `cur_exception`. `data` layout: [0] state, [1] resolve, [2] reject,
// [3] reader, [4] writer, [5] signal.
fn pipe_reject(ctx: &mut ScopedContext, data: &[JSValue], reason: JSValue) -> JSResult {
    if !pipe_mark_settled(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    let res = data[2].call(ctx, JSValue::UNDEFINED, &[reason]);
    release_pipe_locks(ctx, data[3], data[4]);
    res
}

// Settle the pipe promise with a resolution, guarded so it happens exactly once.
fn pipe_resolve(ctx: &mut ScopedContext, data: &[JSValue]) -> JSResult {
    if !pipe_mark_settled(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    let res = data[1].call(ctx, JSValue::UNDEFINED, &[]);
    release_pipe_locks(ctx, data[3], data[4]);
    res
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

// A chunk was read: close on done, otherwise write and continue the loop.
// `data` layout: [0] state, [1] resolve, [2] reject, [3] reader, [4] writer,
// [5] signal.
fn pipe_to_on_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    // Shutdown may have started while this read was in flight; stop the loop.
    if pipe_is_shutting_down(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    let result = cc.arg(0);
    let done = result.get_property_str(ctx, "done")?.to_bool()?;
    if done {
        let prevent_close = pipe_option_bool(ctx, &data[6], "preventClose")?;
        let writer = data[4];
        if prevent_close {
            return pipe_resolve(ctx, &data);
        }
        // Close the destination; resolve the pipe promise when done. A close
        // that throws synchronously (e.g. destination already errored) must
        // not crash the driver — reject the pipe promise instead.
        let close_fn = writer.get_property_str(ctx, "close")?;
        let close_js = match close_fn.call(ctx, writer, &[]) {
            Ok(v) => v,
            Err(_) => {
                let err = ctx.take_exception().to_value(ctx);
                return pipe_reject(ctx, &data, err);
            }
        };
        let mut close_promise: Promise = Promise::try_from(close_js)?;
        let settle_ok = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_to_settle_ok),
                "pipeToSettleOk",
                0,
                0,
                data.into(),
            )?
            .into();
        let settle_err = ctx
            .create_native_function_with_data(
                Callback::Func(pipe_to_settle_err),
                "pipeToSettleErr",
                1,
                0,
                data.into(),
            )?
            .into();
        close_promise.then(ctx, settle_ok, settle_err)?;
        return Ok(JSValue::UNDEFINED);
    }
    let value = result.get_property_str(ctx, "value")?;
    let writer = data[4];
    let write_js = writer
        .get_property_str(ctx, "write")?
        .call(ctx, writer, &[value])?;
    let mut write_promise: Promise = Promise::try_from(write_js)?;
    let loop_fn = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_write_done),
            "pipeToWriteDone",
            1,
            0,
            data.into(),
        )?
        .into();
    let write_fail = ctx
        .create_native_function_with_data(
            Callback::Func(pipe_to_on_write_error),
            "pipeToWriteErr",
            1,
            0,
            data.into(),
        )?
        .into();
    write_promise.then(ctx, loop_fn, write_fail)?;
    Ok(JSValue::UNDEFINED)
}

// A write fulfilled: continue the read loop (unless shutdown began meanwhile).
fn pipe_to_on_write_done(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    if pipe_is_shutting_down(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    pipe_to_read(ctx, &data)
}

// Source read failed: shut the pipe down with the read error. Per spec the
// source's error propagates as the pipe rejection (aborting the writer unless
// prevented, cancelling the reader unless prevented).
fn pipe_to_on_read_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    // If the pipe already settled (or abort already began shutting down), the
    // shutdown is handled by that path — don't run a second one.
    if pipe_is_shutting_down(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    set_state_bool(ctx, &data[0], "shuttingDown", true)?;
    let reason = cc.arg(0);
    pipe_shutdown_with_error(ctx, &data, reason)
}

// Write failed: shut the pipe down with the write error.
fn pipe_to_on_write_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    if pipe_is_shutting_down(ctx, &data[0])? {
        return Ok(JSValue::UNDEFINED);
    }
    set_state_bool(ctx, &data[0], "shuttingDown", true)?;
    let reason = cc.arg(0);
    pipe_shutdown_with_error(ctx, &data, reason)
}

// Shut a pipe down after a read/write error: abort the writer (unless
// prevented) and cancel the reader (unless prevented), then reject the pipe
// promise with the original error. Uses the abort cleanup machinery so the
// final rejection is the action error, or the original error if cleanup runs
// cleanly (per spec, the original error is preferred once cleanup succeeds).
fn pipe_shutdown_with_error(
    ctx: &mut ScopedContext,
    data: &[JSValue],
    reason: JSValue,
) -> JSResult {
    let state = data[0];
    let prevent_abort = pipe_option_bool(ctx, &data[6], "preventAbort")?;
    let prevent_cancel = pipe_option_bool(ctx, &data[6], "preventCancel")?;

    let mut pending = 0;
    if !prevent_abort {
        pending += 1;
    }
    if !prevent_cancel && !pipe_source_is_final(&data[3]) {
        pending += 1;
    }
    if pending == 0 {
        return pipe_reject(ctx, data, reason);
    }

    // For an error shutdown the fallback reason is the original error (not the
    // abort reason); `pipe_shutdown_action_settle` rejects with it once the
    // cleanup actions settle cleanly.
    set_state_num(ctx, &state, "remainingActions", pending)?;
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, "abortErr", JSValue::UNDEFINED)?;
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, "cancelErr", JSValue::UNDEFINED)?;
    state
        .as_object()
        .unwrap()
        .set_property_str(ctx, "fallbackReason", reason)?;

    if !prevent_abort {
        pipe_run_shutdown_action(ctx, data, ShutdownAction::Abort, reason)?;
    }
    if !prevent_cancel && !pipe_source_is_final(&data[3]) {
        pipe_run_shutdown_action(ctx, data, ShutdownAction::Cancel, reason)?;
    }
    Ok(JSValue::UNDEFINED)
}

// Settle the pipe promise once the destination close finishes.
// Success path (close fulfilled): resolve the pipe promise and release locks.
fn pipe_to_settle_ok(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    pipe_resolve(ctx, &data)
}

// Failure path (close rejected): reject the pipe promise with the close error
// and release locks.
fn pipe_to_settle_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = [
        cc.data(0),
        cc.data(1),
        cc.data(2),
        cc.data(3),
        cc.data(4),
        cc.data(5),
        cc.data(6),
    ];
    pipe_reject(ctx, &data, cc.arg(0))
}

fn js_readable_stream_async_iterator(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    create_stream_async_iterator(ctx, &cc.this())
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

/// True if the source stream attached to this pipe's reader is already in a
/// final (errored/closed) state, in which case `reader.cancel()` would only
/// return the stored error without performing a real cancellation. The abort
/// shutdown must then fall back to the abort reason instead of surfacing that
/// stale stored error (WPT "abort signal takes priority over errored readable").
fn pipe_source_is_final(reader: &JSValue) -> bool {
    let Some(r) = JSReadableStreamDefaultReader::payload_of(reader) else {
        return true;
    };
    let Some(stream) = r.stream else {
        return true;
    };
    let Some(p) = stream.payload() else {
        return true;
    };
    p.state != super::RsState::Readable
}

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
        ReadableControllerPayload::new_default(
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
        .set_controller(Some(handle));
    Ok(controller)
}

// ============================================================================
// Stream async iterator
// ============================================================================

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
            ReaderPayload {
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
            ReadableControllerPayload::new_default(JSValue::from(source), Some(stream), 1),
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
            ReadableControllerPayload::new_default(JSValue::from(source), Some(stream), 1),
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
            ReadableControllerPayload::new_default(JSValue::from(source), Some(stream), 1),
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
            ReadableControllerPayload::new_default(JSValue::from(source), Some(stream), 1),
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
            BodySource::Stream(mut stream) => {
                let reader_obj = stream.acquire_reader(ctx, false)?;
                let p = reader_obj
                    .payload()
                    .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultReader"))?;
                let bytes = p.drain_sync(ctx)?;
                let text = String::from_utf8(bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
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
                                .and_then(|cp| cp.stream())
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
