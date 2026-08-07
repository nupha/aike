// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WritableStream and its default writer/controller.

use crate::whatwg::stream::extract_high_water_mark;
use linkme::distributed_slice;
use zuqe::{
    CallContext, Callback, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::{RsState, settle_stream_state, transform::JSTransformStreamDefaultController};

// ============================================================================
// WritableStream
// ============================================================================

pub(crate) struct WritableStreamPayload {
    pub(crate) state: RsState,
    pub(crate) stored_error: JSValue,
    pub(crate) writer: Option<JSWritableStreamDefaultWriter>,
    pub(crate) controller: Option<JSWritableStreamDefaultController>,
}

impl WritableStreamPayload {
    pub(crate) fn new() -> Self {
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
    let queuing_strategy = cc.arg(1);
    let hwm = extract_high_water_mark(ctx, queuing_strategy)?;

    let mut stream = JSWritableStream::new_instance(ctx, new_target, WritableStreamPayload::new())?;
    let controller = JSWritableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        WritableStreamDefaultControllerPayload::new(underlying_sink, Some(stream), hwm),
    )?;

    if let Some(o) = underlying_sink.as_object()
        && let Some(Err(_)) = o.invoke_str_defined(ctx, "start", &[JSValue::from(controller)])?
    {
        let e = ctx.take_exception().to_value(ctx);
        controller.invoke_str(ctx, "error", &[e])?;
    }

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
    if let Some(stream) = cc.this().as_native_object::<JSWritableStream>() {
        settle_stream_state(ctx, stream.into(), RsState::Errored, Some(reason));

        if let Some(p) = stream.payload()
            && let Some(controller) = p.controller
            && let Some(cp) = controller.payload()
            && let Some(sink) = cp.underlying_sink.as_object()
        {
            match sink.invoke_str_callable(ctx, "abort", &[reason]) {
                Ok(None) => {}
                Ok(Some(Ok(v))) => {
                    return if let Ok(_) = Promise::try_from(v) {
                        Ok(v)
                    } else {
                        Ok(JSValue::from(Promise::from_resolve_value(ctx, v)?))
                    };
                }
                Ok(Some(Err(_))) | Err(_) => {
                    let e = ctx.take_exception().to_value(ctx);
                    return Promise::from_reject_value(ctx, e).map(JSValue::from);
                }
            }
        }
    }

    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
}

fn js_writable_stream_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    if let Some(stream) = cc.this().as_native_object::<JSWritableStream>() {
        settle_stream_state(ctx, stream.into(), RsState::Closed, None);
    }
    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
}

// ============================================================================
// WritableStreamDefaultWriter
// ============================================================================

pub(crate) struct WritableStreamDefaultWriterPayload {
    stream: Option<JSWritableStream>,
    pub(crate) closed_promise: Option<Promise>,
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
            if let Some(sink) = cp.underlying_sink.as_object()
                && let Some(Ok(result)) =
                    sink.invoke_str_callable(ctx, "write", &[chunk, controller.into()])?
            {
                if result.is_object() {
                    return Ok(result);
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
                rcp.close_requested.set(true);
                rcp.resolve_pending_read(ctx, JSValue::UNDEFINED, true);
                if rcp.queue.borrow().is_empty()
                    && let Some(mut rs_val) = rcp.stream
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
        rcp.close_requested.set(true);
        rcp.resolve_pending_read(ctx, JSValue::UNDEFINED, true);
        if rcp.queue.borrow().is_empty()
            && let Some(mut rs_val) = rcp.stream
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
    let reason = cc.arg(0);
    let this = cc.this();

    let p = JSWritableStreamDefaultWriter::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultWriter"))?;

    if let Some(stream_val) = p.stream {
        settle_stream_state(ctx, stream_val.into(), RsState::Errored, Some(reason));

        if let Some(sp) = stream_val.payload()
            && let Some(controller) = sp.controller
            && let Some(cp) = controller.payload()
            && let Some(sink) = cp.underlying_sink.as_object()
        {
            match sink.invoke_str_callable(ctx, "abort", &[reason]) {
                Ok(Some(Ok(v))) => {
                    return if let Ok(_) = Promise::try_from(v) {
                        return Ok(v);
                    } else {
                        Ok(JSValue::from(Promise::from_resolve_value(ctx, v)?))
                    };
                }
                Ok(None) => {}
                Ok(Some(Err(_))) | Err(_) => {
                    let e = ctx.take_exception().to_value(ctx);
                    return Promise::from_reject_value(ctx, e).map(JSValue::from);
                }
            }
        }
    }

    Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from)
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

pub(crate) struct WritableStreamDefaultControllerPayload {
    pub(crate) stream: Option<JSWritableStream>,
    pub(crate) underlying_sink: JSValue,
    pub(crate) transform_controller: Option<JSTransformStreamDefaultController>,
    pub(crate) high_water_mark: f64,
}

impl WritableStreamDefaultControllerPayload {
    pub(crate) fn new(
        underlying_sink: JSValue,
        stream: Option<JSWritableStream>,
        high_water_mark: f64,
    ) -> Self {
        Self {
            stream,
            underlying_sink,
            transform_controller: None,
            high_water_mark,
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

fn js_writable_controller_error(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let e = cc.arg(0);
    let this = cc.this();

    if let Some(p) = JSWritableStreamDefaultController::payload_of(&this)
        && let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        // Spec: error() is a no-op once the stream has left the writable state.
        if sp.state != RsState::Readable {
            return Ok(JSValue::UNDEFINED);
        }
        sp.state = RsState::Errored;
        sp.stored_error = e;
    }

    Ok(JSValue::UNDEFINED)
}

// ============================================================================
// Registration
// ============================================================================

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
