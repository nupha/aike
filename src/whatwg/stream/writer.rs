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

use crate::{USER_CLASS_DEFS, whatwg::stream::writable::JSWritableStream};

use super::RsState;

pub(crate) struct WriterPayload {
    pub(super) stream: Option<JSWritableStream>,
    pub(crate) closed_promise: Option<Promise>,
}

impl WriterPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref cp) = self.closed_promise {
            cp.trace(gcx);
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

    /// Release this writer's lock on its stream: detach the stream and clear
    /// the stream's back-reference to this writer.
    pub(crate) fn release_lock(&mut self) {
        if let Some(mut stream_val) = self.stream.take() {
            if let Some(sp) = stream_val.payload_mut() {
                sp.writer = None;
            }
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
                return Promise::from_reject_value(ctx, reason).map(JSValue::from);
            }
            RsState::Closed => {
                let reason = ctx
                    .create_string_from_utf8(b"Writer is closed".to_vec())
                    .map(JSValue::make_string)
                    .unwrap_or(JSValue::UNDEFINED);
                return Promise::from_reject_value(ctx, reason).map(JSValue::from);
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
                return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from);
            }

            // Normal writable path
            let Some(sink) = cp.underlying_sink.as_object() else {
                return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from);
            };
            match sink.invoke_str_callable(ctx, "write", &[chunk, controller.into()])? {
                Some(Ok(v)) => {
                    if v.is_object() {
                        return Ok(v);
                    }
                    // Non-promise result: the write fulfilled immediately.
                    return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from);
                }
                Some(Err(_)) => {
                    // Sink write threw — reject the write with the thrown value
                    // (must take the exception so it does not poison later calls).
                    let e = ctx.take_exception().to_value(ctx);
                    return Promise::from_reject_value(ctx, e).map(JSValue::from);
                }
                None => {
                    // No write method on the sink: write is a fulfilled no-op.
                    return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from);
                }
            }
        }
    }

    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
}

fn js_writer_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSWritableStreamDefaultWriter::payload_of(&this)
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

    JSWritableStreamDefaultWriter::payload_mut_of(&mut this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?
        .abort(ctx, reason)
        .map(JSValue::from)
}

fn js_writer_release_lock(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    if let Some(p) = JSWritableStreamDefaultWriter::payload_mut_of(&mut this) {
        p.release_lock();
    }
    Ok(JSValue::UNDEFINED)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream_default_writer(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStreamDefaultWriter::register(rt)
}
