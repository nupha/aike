// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableByteStreamController: minimal exposure of the byte-stream
//! controller so that `new ReadableStream({type: "bytes"})` works.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, whatwg::stream::readable_stream::JSReadableStream};

use super::{QueueEntry, RsState};

// ============================================================================
// ReadableByteStreamController (minimal exposure)
//
// The full byte-stream engine (byte queue, pull-into, BYOB request lifecycle)
// is not implemented. This class exists so that `ReadableByteStreamController`
// is a visible global constructor and so that `new ReadableStream({ type:
// "bytes" }, strategy)` passes a `ReadableByteStreamController` instance to
// the user's `start(controller)` callback (per WPT construct-byob-request).
// Methods reuse the shared stream-state transitions; end-to-end byte transfer
// through `read()` is intentionally not wired up yet.
// ============================================================================

pub(crate) struct ReadableByteStreamControllerPayload {
    pub(crate) stream: Option<JSReadableStream>,
    pub(crate) underlying_source: JSValue,
    pub(crate) queue: Rc<RefCell<Vec<QueueEntry>>>,
    pub(crate) queue_total_size: Rc<Cell<f64>>,
    pub(crate) high_water_mark: f64,
    pub(crate) close_requested: Cell<bool>,
    /// Pending BYOB request view, if any. Left `None` in this minimal build.
    pub(crate) byob_request: Rc<RefCell<Option<JSValue>>>,
}

impl ReadableByteStreamControllerPayload {
    pub(crate) fn new(
        underlying_source: JSValue,
        stream: Option<JSReadableStream>,
        high_water_mark: f64,
    ) -> Self {
        Self {
            stream,
            underlying_source,
            queue: Rc::new(RefCell::new(Vec::new())),
            queue_total_size: Rc::new(Cell::new(0.0)),
            high_water_mark,
            close_requested: Cell::new(false),
            byob_request: Rc::new(RefCell::new(None)),
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_source.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref br) = *self.byob_request.borrow() {
            br.trace(gcx);
        }
    }
}

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
    // Spec: errored -> null, closed -> 0, else highWaterMark - queueTotalSize.
    if let Some(s) = p.stream
        && let Some(sp) = s.payload()
    {
        match sp.state {
            RsState::Errored => return Ok(JSValue::NULL),
            RsState::Closed => return Ok(JSValue::from(0.0)),
            RsState::Readable => {}
        }
    }
    let total = p.queue_total_size.get();
    Ok(JSValue::from(p.high_water_mark - total))
}

fn js_byte_controller_byob_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;
    // Minimal build: no pending BYOB request is tracked.
    Ok(p.byob_request.borrow().unwrap_or(JSValue::UNDEFINED))
}

fn js_byte_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;

    if p.close_requested.get() {
        return Err(ctx.throw_type_error("cannot enqueue after close"));
    }

    // Minimal build: enqueue the chunk with unit size. Full byte-stream size
    // accounting (byte length) is not wired up yet.
    p.queue.borrow_mut().push(QueueEntry { chunk, size: 1.0 });
    p.queue_total_size.set(p.queue_total_size.get() + 1.0);

    Ok(JSValue::UNDEFINED)
}

fn js_byte_controller_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;

    p.close_requested.set(true);

    if p.queue.borrow().is_empty()
        && let Some(mut rs) = p.stream
    {
        rs.payload_mut()
            .unwrap()
            .settle_state(ctx, RsState::Closed, None);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_byte_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = JSReadableByteStreamController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableByteStreamController"))?;

    if let Some(mut rs) = p.stream {
        rs.payload_mut()
            .unwrap()
            .settle_state(ctx, RsState::Errored, Some(e));
    }

    Ok(JSValue::UNDEFINED)
}

def_native_object!(
    "ReadableByteStreamController",
    JSReadableByteStreamController,
    ReadableByteStreamControllerPayload,
    BYTE_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_byte_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableByteStreamControllerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_byte_stream_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableByteStreamController::register(rt)
}
