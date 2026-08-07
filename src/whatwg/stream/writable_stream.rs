// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WritableStream.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::stream::{
        extract_high_water_mark,
        writable_stream_default_controller::{
            JSWritableStreamDefaultController, WritableStreamDefaultControllerPayload,
        },
        writable_stream_default_writer::{
            JSWritableStreamDefaultWriter, WritableStreamDefaultWriterPayload,
        },
    },
};

use super::{RsState, settle_stream_state};

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

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStream::register(rt)
}
