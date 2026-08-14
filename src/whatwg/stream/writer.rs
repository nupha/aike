// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WritableStreamDefaultWriter: the writer handle returned by
//! `WritableStream.getWriter()`.

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
        writable::JSWritableStream, writable_controller::JSWritableStreamDefaultController,
    },
};

use super::RsState;

pub(crate) struct WriterPayload {
    pub(super) stream: Option<JSWritableStream>,
    pub(crate) closed_promise: Option<Promise>,
    /// `writer.ready`: fulfilled while the stream is writable, rejected on
    /// abort/error. Per the Streams spec, when the stream is aborted the
    /// pending `ready` is rejected in place; if `ready` was already settled,
    /// it is replaced by a fresh rejected promise.
    pub(crate) ready_promise: Option<Promise>,
    /// The promise of the most recent in-flight `write()`, kept so that an
    /// abort/error can reject it (spec: outstanding write() promises reject).
    pub(crate) write_promise: Option<Promise>,
    /// Memoized `writer.abort()` result. Per spec, repeated `writer.abort()`
    /// calls return the *same* promise.
    pub(crate) abort_promise: Option<Promise>,
}

impl WriterPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref cp) = self.closed_promise {
            cp.trace(gcx);
        }
        if let Some(ref rp) = self.ready_promise {
            rp.trace(gcx);
        }
        if let Some(ref wp) = self.write_promise {
            wp.trace(gcx);
        }
        if let Some(ref ap) = self.abort_promise {
            ap.trace(gcx);
        }
    }

    /// Abort the stream this writer is locked to, forwarding `reason`.
    ///
    /// Returns the underlying stream's abort result. If the writer is not
    /// locked to any stream, resolves with `undefined`.
    pub(crate) fn abort(
        &mut self,
        ctx: &mut ScopedContext,
        reason: JSValue,
    ) -> Result<Promise, i32> {
        if let Some(mut ws) = self.stream {
            return ws.payload_mut().unwrap().abort(ctx, reason);
        }

        Promise::from_resolve_value(ctx, JSValue::UNDEFINED)
    }

    /// Reject `writer.ready` per the Streams spec. The pending promise (if
    /// still pending) is rejected in place; an already-settled `ready` is
    /// replaced by a fresh rejected promise.
    pub(crate) fn reject_ready(&mut self, ctx: &mut ScopedContext, reason: JSValue) {
        if self
            .ready_promise
            .as_ref()
            .map(|p| p.state() == zuqe::builtins::promise::PromiseState::Pending)
            .unwrap_or(false)
        {
            // Reject the pending `ready` in place (same reference observed by
            // readers that captured it before the abort).
            if let Some(p) = self.ready_promise.as_mut() {
                p.reject(ctx, reason);
            }
        } else {
            // Already settled (or absent): spec says reset to a *new* rejected
            // promise so that a later read of `writer.ready` observes rejection.
            if let Ok(p) = Promise::from_reject_value(ctx, reason) {
                self.ready_promise = Some(p);
            }
        }
    }

    /// The error used when a writer is released and a monitoring getter
    /// (`closed`/`ready`/`desiredSize`) is accessed. Per the Streams spec
    /// (`defaultWriterLockException`), this is a TypeError: a released writer
    /// can no longer be used to monitor the stream's state.
    fn released_error(ctx: &mut ScopedContext) -> JSValue {
        ctx.create_error_object(
            zuqe::ErrorKind::Type,
            "Writer is released and cannot be used to monitor the stream's closedness",
            false,
        )
        .map(JSValue::from)
        .unwrap_or(JSValue::UNDEFINED)
    }

    /// Release this writer's lock on its stream: detach the stream, clear the
    /// stream's back-reference to this writer, and initialize `closed`/`ready`
    /// to rejected promises (spec `WritableStreamDefaultWriterRelease` invokes
    /// `defaultWriterClosedPromiseInitializeToRejected` / `...Ready...` with a
    /// released-writer TypeError). `desiredSize` throws on access once `stream`
    /// is `None`.
    pub(crate) fn release_lock(&mut self, ctx: &mut ScopedContext) {
        if let Some(mut stream_val) = self.stream.take() {
            if let Some(sp) = stream_val.payload_mut() {
                sp.writer = None;
            }
        }
        let err = Self::released_error(ctx);
        if let Ok(p) = Promise::from_reject_value(ctx, err) {
            self.closed_promise = Some(p);
        }
        let err = Self::released_error(ctx);
        if let Ok(p) = Promise::from_reject_value(ctx, err) {
            self.ready_promise = Some(p);
        }
    }
}

const WRITER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("write", 1, js_writer_write, 0),
    FieldDef::FUNC("close", 0, js_writer_close, 0),
    FieldDef::FUNC("abort", 1, js_writer_abort, 0),
    FieldDef::FUNC("releaseLock", 0, js_writer_release_lock, 0),
    FieldDef::GETSET("closed", Some(js_writer_closed), None, 0),
    FieldDef::GETSET("ready", Some(js_writer_ready), None, 0),
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
    // Released writer: `closed` is a rejected promise (set on release_lock).
    if p.stream.is_none() {
        if let Some(cp) = p.closed_promise {
            return Ok(JSValue::from(cp));
        }
        return Ok(JSValue::UNDEFINED);
    }
    if let Some(cp) = p.closed_promise {
        return Ok(JSValue::from(cp));
    }
    Ok(JSValue::UNDEFINED)
}

fn js_writer_ready(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream writer"))?;
    // Released writer: `ready` is a rejected promise (set on release_lock).
    if p.stream.is_none() {
        if let Some(rp) = p.ready_promise {
            return Ok(JSValue::from(rp));
        }
        return Ok(JSValue::UNDEFINED);
    }
    if let Some(rp) = p.ready_promise {
        return Ok(JSValue::from(rp));
    }
    Ok(JSValue::UNDEFINED)
}

fn js_writer_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStream writer"))?;
    // Released writer: `desiredSize` throws a TypeError (spec:
    // `defaultWriterLockException`).
    let stream_val = p.stream.ok_or_else(|| {
        ctx.throw_type_error(
            "Writer is released and cannot be used to monitor the stream's closedness",
        )
    })?;
    if let Some(sp) = stream_val.payload() {
        // Spec: errored -> null, closed -> 0, else highWaterMark.
        match sp.state {
            RsState::Errored => return Ok(JSValue::NULL),
            RsState::Closed => return Ok(JSValue::ZERO),
            RsState::Readable => {}
        }
        if let Some(ctrl) = sp.controller {
            if let Some(cp) = ctrl.payload() {
                return Ok(JSValue::from(cp.high_water_mark));
            }
        }
    }
    Ok(JSValue::from(1))
}

fn js_writer_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error(
        "WritableStreamDefaultWriter must be obtained via WritableStream.getWriter()",
    ))
}

def_native_object!(
    "WritableStreamDefaultWriter",
    JSWritableStreamDefaultWriter,
    WriterPayload,
    WRITER_PROTO,
    Some(ClassCtorDef::new(js_writer_constructor, 0, false).expose_to_global()),
    on_payload_gc: WriterPayload::gc_trace
);

fn js_writer_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.arg(0);
    let mut this = cc.this();

    let wp = JSWritableStreamDefaultWriter::payload_mut_of(&mut this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;
    let stream_val = wp
        .stream
        .ok_or_else(|| ctx.throw_type_error("writer released"))?;

    // The promise returned by `write()`, kept on the payload so an abort/error
    // can reject any outstanding (in-flight) write.
    let (write_p, write_funcs) = Promise::create(ctx)?;
    wp.write_promise = Some(write_p);

    if let Some(stream) = stream_val.as_native_object::<JSWritableStream>()
        && let Some(sp) = stream.payload()
    {
        // Reject if stream is errored or closed
        match sp.state {
            RsState::Errored => {
                let reason = sp.stored_error;
                write_funcs[1].call(ctx, JSValue::UNDEFINED, &[reason])?;
                return Ok(JSValue::from(write_p));
            }
            RsState::Closed => {
                let reason = ctx
                    .create_string_from_utf8(b"Writer is closed".to_vec())
                    .map(JSValue::make_string)
                    .unwrap_or(JSValue::UNDEFINED);
                write_funcs[1].call(ctx, JSValue::UNDEFINED, &[reason])?;
                return Ok(JSValue::from(write_p));
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

                let mut finish = |ctx: &mut ScopedContext, v: JSValue| -> JSResult {
                    if v.is_object() {
                        // Chain the transform result onto the write promise.
                        let then_fn = v.get_property_str(ctx, "then")?;
                        then_fn.call(ctx, v, &[write_funcs[0], write_funcs[1]])?;
                    } else {
                        write_funcs[0].call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
                    }
                    Ok(JSValue::from(write_p))
                };

                if has_transform {
                    let transform_fn = obj.get_property_str(ctx, "transform").unwrap();
                    let result =
                        transform_fn.call(ctx, tcp.transformer, &[chunk, JSValue::from(tc)])?;
                    return finish(ctx, result);
                } else {
                    // No user-defined transform → pass through: enqueue chunk directly
                    let enqueue_fn =
                        JSValue::from(tcp.readable_controller).get_property_str(ctx, "enqueue")?;
                    enqueue_fn.call(ctx, JSValue::from(tcp.readable_controller), &[chunk])?;
                    write_funcs[0].call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
                    return Ok(JSValue::from(write_p));
                }
            }

            // Normal writable path. Per the Streams spec, `write()` puts
            // `writer.ready` into a pending state for the duration of the
            // underlying sink write, so that callers observe backpressure. The
            // actual sink `write()` call is deferred to a microtask: if the
            // stream is aborted/errored before that microtask runs (e.g. a
            // synchronous `abort()` right after `write()`), the write is
            // discarded and the sink's `write()` is never invoked.
            let (ready, ready_funcs) = Promise::create(ctx)?;
            wp.ready_promise = Some(ready);

            // Capture what the deferred runner needs. The runner re-borrows the
            // controller to re-check the stream state at call time.
            let sink_val = JSValue::from(cp.underlying_sink);
            let runner = ctx.create_native_function_with_data(
                Callback::Func(write_runner),
                "writeRunner",
                0,
                0,
                [
                    chunk,
                    controller.into(),
                    sink_val,
                    write_funcs[0],
                    write_funcs[1],
                    ready_funcs[0],
                    ready_funcs[1],
                ]
                .into(),
            )?;

            // Schedule the sink write on a microtask (Promise.resolve().then).
            let resolved = Promise::from_resolve_value(ctx, JSValue::UNDEFINED)?;
            let then_fn = JSValue::from(resolved).get_property_str(ctx, "then")?;
            then_fn.call(ctx, JSValue::from(resolved), &[runner.into()])?;
            return Ok(JSValue::from(write_p));
        }
    }

    write_funcs[0].call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
    Ok(JSValue::from(write_p))
}

/// Deferred sink `write()` runner. Re-checks the stream state before invoking
/// the sink: if the stream was aborted/errored in the interim (a synchronous
/// `abort()` after `write()`), the write is discarded. Otherwise the sink
/// `write()` result is chained onto both `writer.write`'s promise and `ready`.
fn write_runner(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.data(0);
    let controller_val = cc.data(1);
    let sink_val = cc.data(2);
    let write_resolve = cc.data(3);
    let write_reject = cc.data(4);
    let ready_resolve = cc.data(5);
    let ready_reject = cc.data(6);

    let p = JSWritableStreamDefaultController::payload_of(&controller_val);
    let Some(cp) = p else {
        // Controller gone: reject both promises.
        write_reject.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
        ready_reject.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
        return Ok(JSValue::UNDEFINED);
    };
    let Some(stream) = cp.stream else {
        write_reject.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
        ready_reject.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
        return Ok(JSValue::UNDEFINED);
    };

    // If the stream left the writable state, the write is discarded. The
    // writer's write/ready promises were already rejected by abort/error.
    if let Some(sp) = stream.payload() {
        if sp.state != RsState::Readable {
            return Ok(JSValue::UNDEFINED);
        }
    } else {
        return Ok(JSValue::UNDEFINED);
    }

    if let Some(sink) = sink_val.as_object() {
        match sink.invoke_str_callable(ctx, "write", &[chunk, controller_val])? {
            Some(Ok(v)) => {
                if v.is_object() {
                    let then_fn = v.get_property_str(ctx, "then")?;
                    then_fn.call(ctx, v, &[write_resolve, write_reject])?;
                    let then_fn2 = v.get_property_str(ctx, "then")?;
                    then_fn2.call(ctx, v, &[ready_resolve, ready_reject])?;
                } else {
                    write_resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
                    ready_resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
                }
            }
            Some(Err(_)) => {
                let e = ctx.take_exception().to_value(ctx);
                write_reject.call(ctx, JSValue::UNDEFINED, &[e])?;
                ready_reject.call(ctx, JSValue::UNDEFINED, &[e])?;
            }
            None => {
                write_resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
                ready_resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
            }
        }
    } else {
        write_resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
        ready_resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
    }
    Ok(JSValue::UNDEFINED)
}

fn js_writer_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_mut_of(&mut this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;

    let mut stream_val = p
        .stream
        .ok_or_else(|| ctx.throw_type_error("writer released"))?;

    // Spec: close() must reject (not silently settle) when the stream is
    // already closed or errored.
    if let Some(sp) = stream_val.payload() {
        match sp.state {
            RsState::Errored => {
                return Promise::from_reject_value(ctx, sp.stored_error).map(JSValue::from);
            }
            RsState::Closed => {
                let reason = ctx
                    .create_string_from_utf8(b"cannot close a closed stream".to_vec())
                    .map(JSValue::make_string)
                    .unwrap_or(JSValue::UNDEFINED);
                return Promise::from_reject_value(ctx, reason).map(JSValue::from);
            }
            _ => {}
        }
    }

    // Copy the stream handle before borrowing stream_val via payload_mut, so it
    // can be handed to the async flush handlers below.
    let stream_handle = stream_val;
    if let Some(sp) = stream_val.payload_mut()
        && let Some(controller) = sp.controller
        && let Some(cp) = controller.payload()
    {
        // TransformStream calls `flush`; a plain WritableStream calls `close`.
        // These are mutually exclusive: flush is the close equivalent for a
        // TransformStream, so its underlying sink's close() is never invoked.
        let mut close_err: Option<JSValue> = None;
        let mut flush_promise: Option<Promise> = None;

        if let Some(tc) = cp.transform_controller
            && let Some(tcp) = tc.payload()
            && let Some(tr) = tcp.transformer.as_object()
        {
            // If flush() throws synchronously or returns a rejected promise that
            // is already settled, the close promise must reject with that error.
            match tr.invoke_str_callable(ctx, "flush", &[tc.into()]) {
                Ok(Some(Err(_))) | Err(_) => {
                    close_err = Some(ctx.take_exception().to_value(ctx));
                }
                Ok(Some(Ok(v))) => {
                    // flush may return a promise: await it so an async rejection
                    // also rejects the close promise (spec: flush rejection ->
                    // both sides errored).
                    if let Ok(p) = Promise::try_from(v) {
                        flush_promise = Some(p);
                    }
                }
                _ => {}
            }

            // Synchronous flush (no promise returned): close the readable side now.
            if flush_promise.is_none()
                && let Some(rcp) = tcp.readable_controller.payload()
            {
                rcp.set_close_requested(true);
                rcp.resolve_pending_read(ctx, JSValue::UNDEFINED, true);
                if rcp.queue().borrow().is_empty()
                    && let Some(mut rs_val) = rcp.stream()
                    && let Some(rsp) = rs_val.payload_mut()
                {
                    rsp.state = RsState::Closed;
                }
            }
        } else if let Some(sink) = cp.underlying_sink.as_object()
            && matches!(
                sink.invoke_str_callable(ctx, "close", &[]),
                Ok(Some(Err(_))) | Err(_)
            )
        {
            close_err = Some(ctx.take_exception().to_value(ctx));
        }

        if let Some(e) = close_err {
            sp.state = RsState::Errored;
            sp.stored_error = e;
            return Promise::from_reject_value(ctx, e).map(JSValue::from);
        }

        // flush returned a promise: the close promise settles with it, and the
        // readable side / state are only finalized after flush fulfills.
        if let Some(mut fp) = flush_promise {
            let ok = ctx
                .create_native_function_with_data(
                    Callback::Func(flush_close_ok),
                    "flushCloseOk",
                    0,
                    0,
                    [JSValue::from(stream_handle)].into(),
                )?
                .into();
            let err = ctx
                .create_native_function_with_data(
                    Callback::Func(flush_close_err),
                    "flushCloseErr",
                    1,
                    0,
                    [JSValue::from(stream_handle)].into(),
                )?
                .into();
            fp.then(ctx, ok, err)?;
            return Ok(JSValue::from(fp));
        }

        sp.state = RsState::Closed;
    }

    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
}

// flush fulfilled: close the readable side of the transform stream and mark
// the writable stream closed, per spec (close after flush resolves).
fn flush_close_ok(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut stream_val = cc.data(0);
    if let Some(sp) = JSWritableStream::payload_mut_of(&mut stream_val)
        && let Some(controller) = sp.controller
        && let Some(cp) = controller.payload()
        && let Some(tc) = cp.transform_controller
        && let Some(tcp) = tc.payload()
        && let Some(rcp) = tcp.readable_controller.payload()
    {
        rcp.set_close_requested(true);
        rcp.resolve_pending_read(ctx, JSValue::UNDEFINED, true);
        if rcp.queue().borrow().is_empty()
            && let Some(mut rs_val) = rcp.stream()
            && let Some(rsp) = rs_val.payload_mut()
        {
            rsp.state = RsState::Closed;
        }
        sp.state = RsState::Closed;
    }
    Ok(JSValue::UNDEFINED)
}

// flush rejected: error the writable stream with the flush error. Returning a
// rejected promise makes the close promise reject with that error.
fn flush_close_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut stream_val = cc.data(0);
    let err = cc.arg(0);

    if let Some(sp) = JSWritableStream::payload_mut_of(&mut stream_val)
        && let Some(controller) = sp.controller
        && let Some(cp) = controller.payload()
        && let Some(_tc) = cp.transform_controller
    {
        sp.state = RsState::Errored;
        sp.stored_error = err;
    }
    Promise::from_reject_value(ctx, err).map(JSValue::from)
}

fn js_writer_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let reason = cc.arg(0);

    let p = JSWritableStreamDefaultWriter::payload_mut_of(&mut this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;

    // Released writer: abort returns a rejected promise (spec), not a throw.
    let Some(mut stream) = p.stream else {
        let err = ctx
            .create_error_object(
                zuqe::ErrorKind::Type,
                "Writer is released and cannot be aborted",
                false,
            )
            .map(JSValue::from)
            .unwrap_or(JSValue::UNDEFINED);
        return Promise::from_reject_value(ctx, err).map(JSValue::from);
    };

    // Memoize: repeated `writer.abort()` calls return the *same* promise.
    if let Some(ap) = p.abort_promise {
        return Ok(JSValue::from(ap));
    }

    // `stream.abort()` (below) already rejects `writer.ready` in place, so we
    // must NOT call `reject_ready` again here — a second call would replace the
    // (now-already-rejected) `ready` with a *new* promise, changing the
    // reference observed by an in-flight `writer.ready` capture.

    // Per spec, `writer.abort()` always fulfills with `undefined`, regardless
    // of what the underlying sink's `abort()` returns.
    let sp = stream.payload_mut().unwrap();
    let sink_abort = sp.abort(ctx, reason)?;
    let (abort_p, abort_funcs) = Promise::create(ctx)?;
    p.abort_promise = Some(abort_p);
    let then_fn = JSValue::from(sink_abort).get_property_str(ctx, "then")?;
    let ok_fn = ctx
        .create_native_function_with_data(
            Callback::Func(|ctx, cc| {
                // Always fulfill with undefined. `.then` passes (value, resolve, reject).
                let resolve = cc.arg(1);
                resolve.call(ctx, JSValue::UNDEFINED, &[JSValue::UNDEFINED])?;
                Ok(JSValue::UNDEFINED)
            }),
            "abortThen",
            0,
            0,
            [].into(),
        )?
        .into();
    then_fn.call(ctx, JSValue::from(sink_abort), &[ok_fn, abort_funcs[1]])?;
    Ok(JSValue::from(abort_p))
}

fn js_writer_release_lock(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    if let Some(p) = JSWritableStreamDefaultWriter::payload_mut_of(&mut this) {
        p.release_lock(ctx);
    }
    Ok(JSValue::UNDEFINED)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream_default_writer(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStreamDefaultWriter::register(rt)
}
