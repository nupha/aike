// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WritableStream and its default writer/controller.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

use super::{
    PENDING_READ, RsState, make_iterator_result, make_rejected_promise, make_resolved_promise,
    transform::JSTransformStreamDefaultController,
};

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

pub(crate) struct WritableStreamDefaultControllerPayload {
    pub(crate) stream: Option<JSWritableStream>,
    pub(crate) underlying_sink: JSValue,
    pub(crate) transform_controller: Option<JSTransformStreamDefaultController>,
    pub(crate) high_water_mark: f64,
}

impl WritableStreamDefaultControllerPayload {
    pub(crate) fn new(underlying_sink: JSValue, stream: Option<JSWritableStream>) -> Self {
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
