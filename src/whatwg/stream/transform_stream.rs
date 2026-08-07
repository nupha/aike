// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! TransformStream, wiring a readable and a writable side together.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::stream::{
        extract_high_water_mark,
        readable_stream::{JSReadableStream, ReadableStreamPayload},
        readable_stream_default_controller::{
            JSReadableStreamDefaultController, ReadableStreamDefaultControllerPayload,
        },
        transform_stream_default_controller::{
            JSTransformStreamDefaultController, TransformStreamDefaultControllerPayload,
        },
        writable_stream::{JSWritableStream, WritableStreamPayload},
        writable_stream_default_controller::{
            JSWritableStreamDefaultController, WritableStreamDefaultControllerPayload,
        },
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

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStream::register(rt)
}
