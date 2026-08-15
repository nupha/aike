// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WritableStream.

use linkme::distributed_slice;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::stream::{
        extract_high_water_mark,
        writable_controller::{JSWritableStreamDefaultController, WritableControllerPayload},
        writer::{JSWritableStreamDefaultWriter, WriterPayload},
    },
};

use super::{RsState, notify_peer_closed};

// ============================================================================
// WritableStream
// ============================================================================

pub(crate) struct WritableStreamPayload {
    pub(crate) state: RsState,
    pub(crate) stored_error: JSValue,
    pub(crate) writer: Option<JSWritableStreamDefaultWriter>,
    pub(crate) controller: Option<JSWritableStreamDefaultController>,
    /// Mirrors WHATWG `_started`. The sink `write()` is only ever invoked once
    /// this becomes true (after the `start` algorithm runs). A `write()` issued
    /// before start is enqueued and executed later; `Abort`/`Error` before
    /// start rejects those queued writes and never calls the sink.
    pub(crate) started: bool,
    /// Chunks waiting to be written (mirrors the controller's queue). `write()`
    /// pushes a chunk here when the stream is writable but not yet started, or
    /// when the previous write is still in flight.
    pub(crate) write_queue: Vec<JSValue>,
    /// The `write()` promises corresponding 1:1 to `write_queue` entries.
    pub(crate) write_requests: Vec<Promise>,
    /// The write promise currently in flight (sink `write()` called but not yet
    /// settled). Per spec, an in-flight write is NOT rejected by a later abort.
    pub(crate) in_flight_write_request: Option<Promise>,
    /// Set when `writer.close()` has been requested. The underlying sink's
    /// `close()` is deferred until all queued/in-flight writes have settled.
    pub(crate) close_requested: bool,
    /// The promise returned by `writer.close()`, resolved once the sink's
    /// `close()` completes.
    pub(crate) close_promise: Option<Promise>,
}

impl WritableStreamPayload {
    pub(crate) fn new() -> Self {
        Self {
            state: RsState::Readable,
            stored_error: JSValue::UNDEFINED,
            writer: None,
            controller: None,
            started: false,
            write_queue: Vec::new(),
            write_requests: Vec::new(),
            in_flight_write_request: None,
            close_requested: false,
            close_promise: None,
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
        for c in &self.write_queue {
            c.trace(gcx);
        }
        for p in &self.write_requests {
            p.trace(gcx);
        }
        if let Some(ref p) = self.in_flight_write_request {
            p.trace(gcx);
        }
        if let Some(ref p) = self.close_promise {
            p.trace(gcx);
        }
    }

    /// Set this stream's state (and `stored_error` when errored), then settle
    /// the peer writer's `closed_promise` accordingly.
    ///
    /// A stream can only leave the Readable state once. Re-settling (e.g. abort
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
        // Per spec `WritableStreamFinishErroring`: queued (not yet in-flight)
        // writes are rejected with the stored error. The in-flight write keeps
        // running and is settled by its own sink result.
        let queued = std::mem::take(&mut self.write_requests);
        for mut req in queued {
            req.reject(ctx, reason.unwrap_or(JSValue::UNDEFINED));
        }
        notify_peer_closed(ctx, self.writer.map(JSValue::from), reason);
    }

    /// Reject every queued `write()` promise (those not yet in flight) with
    /// `reason`. Used when the stream errors/aborts before start completes, so a
    /// `write()` issued before start is rejected rather than hanging or calling
    /// the sink.
    pub(crate) fn reject_queued_writes(&mut self, ctx: &mut ScopedContext, reason: JSValue) {
        let queued = std::mem::take(&mut self.write_requests);
        for mut req in queued {
            req.reject(ctx, reason);
        }
        self.write_queue.clear();
    }

    /// Mirror of WHATWG `WritableStreamDefaultControllerAdvanceQueueIfNeeded`.
    /// If the stream is writable & started and no write is in flight, pop the
    /// first queued chunk and invoke the sink `write()` (the in-flight write).
    pub(crate) fn advance_queue(&mut self, ctx: &mut ScopedContext) -> Result<(), i32> {
        if self.in_flight_write_request.is_some() {
            return Ok(());
        }
        if !self.started {
            return Ok(());
        }
        if self.state != RsState::Readable {
            return Ok(());
        }
        if self.write_queue.is_empty() {
            // No more writes: if a close was requested, perform it now (the sink's
            // `close()` runs once the queue has fully drained).
            if self.close_requested {
                self.try_close(ctx)?;
            }
            return Ok(());
        }
        let chunk = self.write_queue.remove(0);
        let write_promise = self.write_requests.remove(0);
        self.in_flight_write_request = Some(write_promise);
        self.process_write(ctx, chunk)
    }

    /// Settle the stream's deferred close promise. On success resolve it; on
    /// failure mark the stream Errored with `reason` and reject it.
    fn finish_close(
        ctx: &mut ScopedContext,
        stream_val: &mut JSValue,
        ok: bool,
        value: JSValue,
    ) -> JSResult {
        if let Some(mut sp) = JSWritableStream::payload_mut_of(stream_val) {
            if ok {
                if let Some(mut cp) = sp.close_promise.take() {
                    let _ = cp.resolve(ctx, JSValue::UNDEFINED);
                }
            } else {
                sp.state = RsState::Errored;
                sp.stored_error = value;
                if let Some(mut cp) = sp.close_promise.take() {
                    let _ = cp.reject(ctx, value);
                }
                // The stream is now errored: reject writer.ready with the error.
                if let Some(mut w) = sp.writer {
                    let mut wv = JSValue::from(w);
                    if let Some(mut wp) = JSWritableStreamDefaultWriter::payload_mut_of(&mut wv) {
                        wp.reject_ready(ctx, value);
                    }
                }
            }
        }
        Ok(JSValue::UNDEFINED)
    }

    /// Perform the underlying sink `close()` now (all writes have drained), and
    /// settle the close promise. Per spec `WritableStreamClose`, `close()` is
    /// only invoked once the stream is no longer writable/backpressured.
    fn perform_close(&mut self, ctx: &mut ScopedContext) -> Result<(), i32> {
        self.state = RsState::Closed;
        // `stream_val` is needed by the async close handlers; recover it via the
        // writer's back-reference.
        let stream_val = match self.writer {
            Some(w) => {
                let wv = JSValue::from(w);
                match JSWritableStreamDefaultWriter::payload_of(&wv) {
                    Some(wp) => match wp.stream {
                        Some(s) => JSValue::from(s),
                        None => JSValue::UNDEFINED,
                    },
                    None => JSValue::UNDEFINED,
                }
            }
            None => JSValue::UNDEFINED,
        };
        let sink_close = match self.controller {
            Some(c) => {
                let cv = JSValue::from(c);
                match JSWritableStreamDefaultController::payload_of(&cv) {
                    Some(cp) => cp.underlying_sink.as_object(),
                    None => None,
                }
            }
            None => None,
        };
        let sink_close = match sink_close {
            Some(s) => s,
            None => {
                if let Some(mut cp) = self.close_promise.take() {
                    cp.resolve(ctx, JSValue::UNDEFINED);
                }
                return Ok(());
            }
        };
        match sink_close.invoke_str_callable(ctx, "close", &[]) {
            Ok(Some(Ok(v))) => {
                if let Ok(mut p) = Promise::try_from(v) {
                    let on_ok = ctx.create_native_function_with_data(
                        Callback::Func(|ctx, cc| {
                            let mut sv = cc.data(0);
                            Self::finish_close(ctx, &mut sv, true, JSValue::UNDEFINED)
                        }),
                        "closeOk",
                        0,
                        0,
                        [stream_val].into(),
                    )?;
                    let on_err = ctx.create_native_function_with_data(
                        Callback::Func(|ctx, cc| {
                            let mut sv = cc.data(0);
                            Self::finish_close(ctx, &mut sv, false, cc.arg(0))
                        }),
                        "closeErr",
                        0,
                        0,
                        [stream_val].into(),
                    )?;
                    p.then(ctx, on_ok.into(), on_err.into())?;
                } else if let Some(mut cp) = self.close_promise.take() {
                    cp.resolve(ctx, JSValue::UNDEFINED);
                }
            }
            Ok(Some(Err(_))) => {
                let e = ctx.take_exception().to_value(ctx);
                self.state = RsState::Errored;
                self.stored_error = e;
                if let Some(mut cp) = self.close_promise.take() {
                    cp.reject(ctx, e);
                }
                if let Some(mut w) = self.writer {
                    let mut wv = JSValue::from(w);
                    if let Some(mut wp) = JSWritableStreamDefaultWriter::payload_mut_of(&mut wv) {
                        wp.reject_ready(ctx, e);
                    }
                }
            }
            _ => {
                if let Some(mut cp) = self.close_promise.take() {
                    cp.resolve(ctx, JSValue::UNDEFINED);
                }
            }
        }
        Ok(())
    }

    /// If a `close()` was requested and there are no pending/in-flight writes,
    /// perform it now; otherwise it will run once the last write settles.
    pub(crate) fn try_close(&mut self, ctx: &mut ScopedContext) -> Result<(), i32> {
        if self.close_requested
            && self.write_queue.is_empty()
            && self.in_flight_write_request.is_none()
        {
            self.perform_close(ctx)?;
        }
        Ok(())
    }

    /// Mirror of WHATWG `WritableStreamDefaultControllerProcessWrite`: invoke the
    /// sink's `write(chunk)` synchronously and chain its result onto the
    /// in-flight write promise (and the writer's `ready` promise).
    fn process_write(&mut self, ctx: &mut ScopedContext, chunk: JSValue) -> Result<(), i32> {
        let controller_val = match self.controller {
            Some(c) => JSValue::from(c),
            None => {
                if let Some(mut p) = self.in_flight_write_request.take() {
                    p.reject(ctx, JSValue::UNDEFINED);
                }
                return Ok(());
            }
        };
        let stream_val = match JSWritableStreamDefaultController::payload_of(&controller_val)
            .and_then(|cp| cp.stream)
        {
            Some(s) => JSValue::from(s),
            None => {
                if let Some(mut p) = self.in_flight_write_request.take() {
                    p.reject(ctx, JSValue::UNDEFINED);
                }
                return Ok(());
            }
        };
        let sink_val = match JSWritableStreamDefaultController::payload_of(&controller_val)
            .map(|cp| cp.underlying_sink)
        {
            Some(s) => s,
            None => {
                if let Some(mut p) = self.in_flight_write_request.take() {
                    p.reject(ctx, JSValue::UNDEFINED);
                }
                return Ok(());
            }
        };

        // writer.ready goes pending (backpressure) for the duration of the
        // in-flight write: replace it with a fresh pending promise. `finish` and
        // `reject_ready` settle the current `ready_promise` in place.
        if let Some(mut writer) = self.writer
            && let Some(mut wp) = writer.payload_mut()
        {
            let (ready, _ready_funcs) = Promise::create(ctx)?;
            wp.ready_promise = Some(ready);
            let (write_p, write_funcs) = Promise::create(ctx)?;
            // Swap the placeholder in_flight_write_request with the real promise.
            let _ = self.in_flight_write_request.replace(write_p);
            let resolve = write_funcs[0];
            let reject = write_funcs[1];
            Self::chain_write_result(
                ctx,
                sink_val,
                chunk,
                controller_val,
                stream_val,
                resolve,
                reject,
            )?;
        }
        Ok(())
    }

    /// Resolve/reject the in-flight write & ready promises, clear the in-flight
    /// slot, then advance the queue. Handles (resolve, reject, stream_val) live
    /// in `cc.data(0..2)`; `cc.data(3)` is the ok flag (`undefined` = ok, anything
    /// else = error); the value/reason is `cc.arg(0)`. On success the writer's
    /// current `ready_promise` is resolved in place (stream ready again).
    fn finish_in_flight_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
        let ok = cc.data(3).is_undefined();
        let value = cc.arg(0);
        let resolve = cc.data(0);
        let reject = cc.data(1);
        let mut stream_val = cc.data(2);
        if ok {
            resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
        } else {
            reject.call(ctx, JSValue::UNDEFINED, &[value])?;
        }
        if let Some(mut sp) = JSWritableStream::payload_mut_of(&mut stream_val) {
            sp.in_flight_write_request = None;
            // Stream is ready again: resolve writer.ready in place.
            if let Some(mut w) = sp.writer
                && let Some(mut wp) = w.payload_mut()
                && let Some(mut rp) = wp.ready_promise
            {
                rp.resolve(ctx, JSValue::UNDEFINED);
            }
            sp.advance_queue(ctx)?;
        }
        Ok(JSValue::UNDEFINED)
    }

    /// Call `sink.write(chunk)` and chain its outcome onto the in-flight write
    /// promise (`resolve`/`reject`). On completion, advance the queue for the
    /// next queued chunk.
    fn chain_write_result(
        ctx: &mut ScopedContext,
        sink_val: JSValue,
        chunk: JSValue,
        controller_val: JSValue,
        stream_val: JSValue,
        resolve: JSValue,
        reject: JSValue,
    ) -> Result<(), i32> {
        // `data` layout: [resolve, reject, stream_val, ok_flag].
        // ok_flag is `undefined` for the success path (ok); a non-undefined value
        // (here 1) marks the error path, whose reason comes from `cc.arg(0)`.
        let mut data: Vec<JSValue> = [resolve, reject, stream_val, JSValue::UNDEFINED].to_vec();
        let mut data_err = data.clone();
        data_err[3] = JSValue::from(1);
        if let Some(sink) = sink_val.as_object() {
            match sink.invoke_str_callable(ctx, "write", &[chunk, controller_val])? {
                Some(Ok(v)) => {
                    // Per spec `uponPromise`, the sink's return value is always
                    // treated as a promise, so the in-flight write settles on a
                    // microtask even when the sink returns synchronously. This
                    // lets a synchronous `abort()` (issued right after `write()`)
                    // take effect before the write completes, so queued writes
                    // are rejected instead of invoking the sink.
                    let mut p = if let Ok(mut p) = Promise::try_from(v) {
                        p
                    } else {
                        Promise::from_resolve_value(ctx, v)?
                    };
                    let on_ok = ctx.create_native_function_with_data(
                        Callback::Func(Self::finish_in_flight_write),
                        "writeOk",
                        0,
                        0,
                        data.into(),
                    )?;
                    let on_err = ctx.create_native_function_with_data(
                        Callback::Func(Self::finish_in_flight_write),
                        "writeErr",
                        0,
                        0,
                        data_err.into(),
                    )?;
                    p.then(ctx, on_ok.into(), on_err.into())?;
                }
                Some(Err(_)) => {
                    let e = ctx.take_exception().to_value(ctx);
                    let on_err = ctx.create_native_function_with_data(
                        Callback::Func(Self::finish_in_flight_write),
                        "writeErr",
                        0,
                        0,
                        data_err.into(),
                    )?;
                    let noop = ctx.create_native_function_with_data(
                        Callback::Func(|_ctx, _cc| Ok(JSValue::UNDEFINED)),
                        "writeNoop",
                        0,
                        0,
                        [].into(),
                    )?;
                    // Reject via a microtask so abort can interleave.
                    let mut p = Promise::from_reject_value(ctx, e)?;
                    p.then(ctx, noop.into(), on_err.into())?;
                }
                None => {
                    let mut p = Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?;
                    let on_ok = ctx.create_native_function_with_data(
                        Callback::Func(Self::finish_in_flight_write),
                        "writeOk",
                        0,
                        0,
                        data.into(),
                    )?;
                    p.then(ctx, on_ok.into(), on_ok.into())?;
                }
            }
        } else {
            // No underlying sink object: treat as an immediately-fulfilled write.
            let mut p = Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?;
            let on_ok = ctx.create_native_function_with_data(
                Callback::Func(Self::finish_in_flight_write),
                "writeOk",
                0,
                0,
                data.into(),
            )?;
            p.then(ctx, on_ok.into(), on_ok.into())?;
        }
        Ok(())
    }

    /// Abort this writable stream: settle it as Errored with `reason`, then
    /// propagate the underlying sink's `abort()` result (including a rejection)
    /// to the caller. Per spec `WritableStreamAbort`, aborting an already
    /// errored/closed stream is a no-op that resolves with undefined.
    pub(crate) fn abort(
        &mut self,
        ctx: &mut ScopedContext,
        reason: JSValue,
    ) -> Result<Promise, i32> {
        if self.state != RsState::Readable {
            return Promise::from_resolve_value(ctx, JSValue::UNDEFINED);
        }
        self.settle_state(ctx, RsState::Errored, Some(reason));

        // Per the Streams spec, aborting a WritableStream synchronously signals
        // the controller's `signal` and rejects the writer's `ready` promise.
        // This must happen before invoking the underlying sink's `abort()` so
        // observers see a consistent state.
        if let Some(controller) = self.controller
            && let Some(cp) = controller.payload()
        {
            if let Some(signal) = cp.signal {
                crate::whatwg::abort::abort_signal(ctx, signal, reason)?;
            }
        }
        if let Some(mut writer) = self.writer
            && let Some(mut wp) = writer.payload_mut()
        {
            wp.reject_ready(ctx, reason);
        }

        if let Some(controller) = self.controller
            && let Some(cp) = controller.payload()
            && let Some(sink) = cp.underlying_sink.as_object()
        {
            match sink.invoke_str_defined(ctx, "abort", &[reason]) {
                Ok(Some(Ok(v))) => {
                    return if let Ok(p) = Promise::try_from(v) {
                        Ok(p)
                    } else {
                        Promise::from_resolve_value(ctx, v)
                    };
                }
                Ok(None) => {}
                Ok(Some(Err(_))) | Err(_) => {
                    let e = ctx.take_exception().to_value(ctx);
                    return Promise::from_reject_value(ctx, e);
                }
            }
        }

        Promise::from_resolve_value(ctx, JSValue::UNDEFINED)
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

impl JSWritableStream {
    /// Lock this stream by creating and installing a default writer.
    ///
    /// Errors with TypeError if the stream is already locked. The returned
    /// writer settles its `closed_promise` from the current `state`
    /// (`stored_error` on Errored).
    pub(crate) fn acquire_writer(
        &mut self,
        ctx: &mut ScopedContext,
    ) -> Result<JSWritableStreamDefaultWriter, i32> {
        let handle = *self;

        let sp = self
            .payload_mut()
            .ok_or_else(|| ctx.throw_type_error("not a WritableStream"))?;

        if sp.writer.is_some() {
            return Err(ctx.throw_type_error("WritableStream is already locked"));
        }

        let mut closed_promise = Promise::create(ctx)?.0;
        match sp.state {
            RsState::Closed => closed_promise.resolve(ctx, JSValue::UNDEFINED),
            RsState::Errored => closed_promise.reject(ctx, sp.stored_error),
            RsState::Readable => {}
        }

        // `writer.ready` is a single persistent promise. It starts pending and
        // is resolved once the stream is ready to accept writes (after start,
        // and after each in-flight write completes), and rejected on abort.
        let ready_promise = Promise::create(ctx)?.0;

        let writer = JSWritableStreamDefaultWriter::new_instance(
            ctx,
            JSValue::UNDEFINED,
            WriterPayload {
                stream: Some(handle),
                closed_promise: Some(closed_promise),
                ready_promise: Some(ready_promise),
                write_promise: None,
                abort_promise: None,
            },
        )?;

        sp.writer = Some(writer);

        Ok(writer)
    }
}

fn js_writable_stream_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let underlying_sink = cc.arg(0);
    let queuing_strategy = cc.arg(1);
    let hwm = extract_high_water_mark(ctx, queuing_strategy)?;

    let mut stream = JSWritableStream::new_instance(ctx, new_target, WritableStreamPayload::new())?;
    let controller = JSWritableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        WritableControllerPayload::new(underlying_sink, Some(stream), hwm),
    )?;

    // SetUpWritableStreamDefaultControllerFromUnderlyingSink: capture the sink's
    // `write`/`close`/`abort` callbacks once, so a throwing getter re-throws from
    // the constructor and later writes/closes/aborts do not re-run the getter. A
    // present but non-callable callback member throws TypeError (WebIDL).
    if let Some(o) = underlying_sink.as_object() {
        for method in ["write", "close", "abort"] {
            let v = o.get_property_str(ctx, method)?;
            if !v.is_undefined() && !v.is_callable() {
                let msg = [method, " must be a function"].concat();
                return Err(ctx.throw_type_error(&msg));
            }
        }
    }

    // Invoke the sink `start(controller)` **synchronously** so the controller is
    // observable immediately (e.g. `start(c) { ctrl = c }`). Per spec the stream
    // is only considered "started" (`_started === true`) after a microtask, so
    // `write()`s issued before that point are enqueued rather than invoking the
    // sink. A thrown `start()` is re-thrown by the WritableStream() constructor
    // (spec: "Any exceptions thrown by start will be re-thrown by the
    // WritableStream() constructor"). `invoke_str_defined` returns `Ok(Some(Err))`
    // when start throws, so the inner JSResult must be unwrapped via `?`.
    if let Some(o) = underlying_sink.as_object() {
        if let Some(r) = o.invoke_str_defined(ctx, "start", &[JSValue::from(controller)])? {
            r?;
        }
    }

    // Defer `started = true` + queue advance to a microtask, so a `write()`
    // issued synchronously after construction is enqueued (matching spec
    // `_started` gating) and a following synchronous `abort()` can reject it
    // before the sink `write()` is ever called.
    let start_fn = ctx.create_native_function_with_data(
        Callback::Func(|_ctx, cc| {
            let mut stream_val = cc.data(0);
            if let Some(sp) = JSWritableStream::payload_mut_of(&mut stream_val) {
                sp.started = true;
                // The stream is now ready to accept writes: resolve writer.ready.
                if let Some(mut w) = sp.writer
                    && let Some(wp) = w.payload_mut()
                    && let Some(mut rp) = wp.ready_promise
                {
                    rp.resolve(_ctx, JSValue::UNDEFINED);
                }
                sp.advance_queue(_ctx)?;
            }
            Ok(JSValue::UNDEFINED)
        }),
        "writableStart",
        0,
        0,
        [JSValue::from(stream)].into(),
    )?;
    let resolved = Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?;
    let then_fn = JSValue::from(resolved).get_property_str(ctx, "then")?;
    then_fn.call(ctx, JSValue::from(resolved), &[start_fn.into()])?;

    stream.payload_mut().unwrap().controller = Some(controller);

    Ok(stream.into())
}

fn js_writable_stream_locked(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream"))?;
    Ok(JSValue::from(p.writer.is_some()))
}

fn js_writable_stream_get_writer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut stream = cc
        .this()
        .as_native_object::<JSWritableStream>()
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream"))?;

    stream.acquire_writer(ctx).map(JSValue::from)
}

fn js_writable_stream_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let reason = cc.arg(0);

    let sp = JSWritableStream::payload_mut_of(&mut this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream"))?;

    sp.abort(ctx, reason).map(JSValue::from)
}

fn js_writable_stream_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    if let Some(stream) = JSWritableStream::payload_mut_of(&mut this) {
        stream.settle_state(ctx, RsState::Closed, None);
    }
    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStream::register(rt)
}
