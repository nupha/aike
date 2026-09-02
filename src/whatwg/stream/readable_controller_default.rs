// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::{ReadableControllerPayload, readable_controller::get_readable_controller};

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
    ReadableControllerPayload,
    CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_controller_constructor, 0, false).expose_to_global())
);

fn js_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let options = cc.arg(1);
    let p = get_readable_controller(ctx, &this)?;
    // Spec `enqueue(chunk) method steps`: if CanCloseOrEnqueue is false, throw a
    // TypeError (stream closed/errored or close requested).
    if !p.can_close_or_enqueue() {
        return Err(ctx.throw_type_error(
            "Cannot enqueue on a ReadableStreamDefaultController whose stream is not readable",
        ));
    }
    p.enqueue(ctx, chunk, options)
}

fn js_controller_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_readable_controller(ctx, &this)?;
    p.close(ctx)?;
    Ok(JSValue::UNDEFINED)
}

fn js_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = get_readable_controller(ctx, &this)?;
    p.error(ctx, e)?;
    Ok(JSValue::UNDEFINED)
}

fn js_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = get_readable_controller(ctx, &this)?;

    // Spec: errored -> null, closed -> 0, else highWaterMark - queueTotalSize
    // (can be negative when the queue is over-full).
    let size = p.desired_size(ctx)?;
    if size.is_nan() {
        Ok(JSValue::NULL)
    } else {
        Ok(JSValue::from(size))
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStreamDefaultController::register(rt)
}
