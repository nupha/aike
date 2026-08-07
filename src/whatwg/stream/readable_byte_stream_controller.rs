// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableByteStreamController.
//!
//! This is a minimal implementation: it exists so that `ReadableByteStreamController`
//! is a visible global constructor and so that `new ReadableStream({ type:
//! "bytes" }, strategy)` passes a `ReadableByteStreamController` instance to
//! the user's `start(controller)` callback (per WPT construct-byob-request).
//! Methods reuse the shared stream-state transitions; end-to-end byte transfer
//! through `read()` is intentionally not wired up yet.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

use super::{ReadableControllerPayload, RsState};

const BYTE_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("enqueue", 1, js_byte_controller_enqueue, 0),
    FieldDef::FUNC("close", 0, js_byte_controller_close, 0),
    FieldDef::FUNC("error", 1, js_byte_controller_error, 0),
    FieldDef::GETSET(
        "desiredSize",
        Some(js_byte_controller_desired_size),
        None,
        0,
    ),
    FieldDef::GETSET(
        "byobRequest",
        Some(js_byte_controller_byob_request),
        None,
        0,
    ),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableByteStreamController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_byte_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("ReadableByteStreamController cannot be constructed directly"))
}

fn js_byte_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    let size = p.desired_size(ctx)?;
    if size == u32::MAX {
        Ok(JSValue::NULL)
    } else {
        Ok(JSValue::from(size))
    }
}

fn js_byte_controller_byob_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    // Minimal build: no pending BYOB request is tracked.
    Ok(p.byob_request())
}

fn js_byte_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    p.enqueue(ctx, chunk)
}

fn js_byte_controller_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    p.close(ctx)?;
    Ok(JSValue::UNDEFINED)
}

fn js_byte_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    p.error(ctx, e)?;
    Ok(JSValue::UNDEFINED)
}

def_native_object!(
    "ReadableByteStreamController",
    JSReadableByteStreamController,
    ReadableControllerPayload,
    BYTE_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_byte_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableControllerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_byte_stream_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableByteStreamController::register(rt)
}
