// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! TransformStream and its default controller, wiring a readable and a
//! writable side together.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, whatwg::stream::extract_high_water_mark};

use super::{
    QueueEntry, RsState,
    readable::{
        JSReadableStream, JSReadableStreamDefaultController,
        ReadableStreamDefaultControllerPayload, ReadableStreamPayload,
    },
    writable::{
        JSWritableStream, JSWritableStreamDefaultController,
        WritableStreamDefaultControllerPayload, WritableStreamPayload,
    },
};

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

    // Per the WHATWG spec the second/third arguments are the writable and
    // readable queuing strategies respectively.
    let writable_strategy = cc.arg(1);
    let readable_strategy = cc.arg(2);
    let writable_hwm = extract_high_water_mark(ctx, writable_strategy)?;
    let readable_hwm = extract_high_water_mark(ctx, readable_strategy)?;

    // Readable side
    let mut readable =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
    let rc = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableStreamDefaultControllerPayload::new(
            JSValue::UNDEFINED,
            Some(readable),
            readable_hwm,
        ),
    )?;
    readable.payload_mut().unwrap().controller.replace(rc);

    // Writable side
    let mut writable =
        JSWritableStream::new_instance(ctx, JSValue::UNDEFINED, WritableStreamPayload::new())?;
    let wc_payload =
        WritableStreamDefaultControllerPayload::new(transformer, Some(writable), writable_hwm);
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

    // Call transformer.start(transformController) if present. A throw propagates
    // via `?` so the construction fails and the caller sees the error.
    if let Some(obj) = transformer.as_object()
        && let Ok(start_fn) = obj.get_property_str(ctx, "start")
        && start_fn.is_callable()
        && let Some(tc) = wc.payload().and_then(|p| p.transform_controller)
    {
        start_fn.call(ctx, transformer, &[JSValue::from(tc)])?;
    }

    Ok(instance.into())
}

fn js_transform_stream_readable(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStream"))?;
    Ok(p.readable.map_or(JSValue::UNDEFINED, JSValue::from))
}

fn js_transform_stream_writable(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStream"))?;
    Ok(p.writable.map_or(JSValue::UNDEFINED, JSValue::from))
}

// ============================================================================
// TransformStreamDefaultController
// ============================================================================

pub(crate) struct TransformStreamDefaultControllerPayload {
    pub(crate) readable_controller: JSReadableStreamDefaultController,
    pub(crate) writable_stream: JSWritableStream,
    pub(crate) transformer: JSValue,
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

    let resolved = rcp.resolve_pending_read(ctx, chunk, false);

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
        rcp.reject_pending_read(ctx, reason);

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

        rcp.resolve_pending_read(ctx, JSValue::UNDEFINED, true);

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
        // Spec: if the readable stream is errored -> null; closed -> 0;
        // else highWaterMark - queueTotalSize.
        if let Some(s) = rcp.stream
            && let Some(sp) = s.payload()
        {
            match sp.state {
                RsState::Errored => return Ok(JSValue::NULL),
                RsState::Closed => return Ok(JSValue::ZERO),
                RsState::Readable => {}
            }
        }
        let total = rcp.queue_total_size.get();
        return Ok(JSValue::from(rcp.high_water_mark - total));
    }
    Ok(JSValue::ZERO)
}

// ============================================================================
// Registration
// ============================================================================

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStream::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStreamDefaultController::register(rt)
}
