// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! TransformStreamDefaultController: the controller handed to a transformer's
//! `start()`/`transform()`/`flush()` callbacks.

use std::{cell::Cell, rc::Rc};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, Exception, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::stream::{
        readable_controller_default::JSReadableStreamDefaultController,
        transform::JSTransformStream, writable::JSWritableStream,
    },
};

use super::RsState;

pub(crate) struct TransformControllerPayload {
    /// Back-reference to the controlling TransformStream (spec `[[stream]]`).
    pub(crate) stream: Option<JSTransformStream>,
    pub(crate) readable_controller: JSReadableStreamDefaultController,
    pub(crate) writable_stream: JSWritableStream,
    pub(crate) transformer: JSValue,
    /// Captured `transformer.cancel` algorithm (spec `[[cancelAlgorithm]]`).
    pub(crate) cancel_algorithm: JSValue,
    /// Captured `transformer.flush` algorithm (spec `[[flushAlgorithm]]`).
    pub(crate) flush_algorithm: JSValue,
    /// Captured `transformer.transform` algorithm (spec `[[transformAlgorithm]]`).
    pub(crate) transform_algorithm: JSValue,
    /// Set once cancel/flush is invoked; subsequent calls return this promise
    /// (spec `[[finishPromise]]`).
    pub(crate) finish_promise: Option<zuqe::builtins::promise::Promise>,
    /// Shared `[[backpressure]]` — must be `Rc<Cell>` because `def_native_object!`
    /// payloads are Copy, so each `payload()` call returns a fresh copy.
    /// Writing to one copy doesn't affect others; an `Rc<Cell>` ensures all
    /// copies share the same state.
    pub(crate) backpressure: Rc<Cell<bool>>,
    /// Shared `[[backpressureChangePromise]]`.
    pub(crate) backpressure_change_promise: Rc<Cell<Option<zuqe::builtins::promise::Promise>>>,
}

impl TransformControllerPayload {
    pub(super) fn new(
        readable_controller: JSReadableStreamDefaultController,
        writable_stream: JSWritableStream,
        transformer: JSValue,
    ) -> Self {
        Self {
            stream: None,
            readable_controller,
            writable_stream,
            transformer,
            cancel_algorithm: JSValue::UNDEFINED,
            flush_algorithm: JSValue::UNDEFINED,
            transform_algorithm: JSValue::UNDEFINED,
            finish_promise: None,
            backpressure: Rc::new(Cell::new(true)),
            backpressure_change_promise: Rc::new(Cell::new(None)),
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        self.readable_controller.trace(gcx);
        self.writable_stream.trace(gcx);
        self.transformer.trace(gcx);
        self.cancel_algorithm.trace(gcx);
        self.flush_algorithm.trace(gcx);
        self.transform_algorithm.trace(gcx);
        if let Some(ref p) = self.finish_promise {
            p.trace(gcx);
        }
        if let Some(p) = self.backpressure_change_promise.get() {
            p.trace(gcx);
        }
    }
}

impl GcTrace for TransformControllerPayload {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        self.gc_trace(gcx);
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
    TransformControllerPayload,
    TRANSFORM_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_transform_controller_constructor, 0, false).expose_to_global())
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

    // Spec §6.4.2 step 3: once the readable side is errored or close has been
    // requested, enqueue throws a TypeError.
    if !rcp.can_close_or_enqueue() {
        return Err(ctx.throw_type_error("cannot enqueue on a closed or errored stream"));
    }

    // Spec §6.4.2 steps 4-5: delegate to the readable controller (which runs the
    // strategy's size algorithm). When that throws, error the writable side,
    // unblock write, and re-throw the readable side's stored error — the first
    // error wins over a later one thrown by size().
    if rcp.enqueue(ctx, chunk, JSValue::UNDEFINED).is_err() {
        let e = ctx.take_exception().to_value(ctx);
        if let Some(stream_handle) = p.stream {
            let stream_val = JSValue::from(stream_handle);
            super::transform::transform_stream_error_writable_and_unblock_write(
                ctx,
                &stream_val,
                e,
            )?;
        }
        let stored = match rcp.stream() {
            Some(s) => {
                let sv = JSValue::from(s);
                super::JSReadableStream::payload_of(&sv)
                    .map(|sp| sp.stored_error)
                    .unwrap_or(e)
            }
            None => e,
        };
        ctx.throw(Exception::Error(stored));
        return Err(-1);
    }

    // Propagate backpressure: if the readable side's desiredSize ≤ 0, the
    // TransformStream should block further writes (spec §6.4.2 step 7).
    if let Some(stream_handle) = p.stream {
        let stream_val = JSValue::from(stream_handle);
        let readable_full = rcp.desired_size(ctx)? <= 0.0;
        if readable_full && !p.backpressure.get() {
            super::transform::transform_stream_set_backpressure(ctx, &stream_val, true)?;
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_transform_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let reason = cc.arg(0);
    let p = JSTransformStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStreamDefaultController"))?;

    // TransformStreamDefaultControllerError(controller, e) →
    //   TransformStreamError(controller.[[stream]], e) →
    //   1. ReadableStreamDefaultControllerError(readable.[[controller]], e)
    //   2. TransformStreamErrorWritableAndUnblockWrite(stream, e)
    if let Some(rcp) = p.readable_controller.payload() {
        rcp.error(ctx, reason)?;
    }
    if let Some(stream_handle) = p.stream {
        let stream_val = JSValue::from(stream_handle);
        super::transform::transform_stream_error_writable_and_unblock_write(
            ctx,
            &stream_val,
            reason,
        )?;
    }

    Ok(JSValue::UNDEFINED)
}

fn js_transform_controller_terminate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStreamDefaultController"))?;

    // Spec `TransformStreamDefaultControllerTerminate` step 3: close the
    // readable side.
    if let Some(rcp) = p.readable_controller.payload() {
        rcp.set_close_requested(true);

        rcp.resolve_pending_read(ctx, JSValue::UNDEFINED, true);

        if rcp.queue().borrow().is_empty()
            && let Some(mut stream_val) = rcp.stream()
            && let Some(sp) = stream_val.payload_mut()
        {
            sp.state = RsState::Closed;
        }
    }

    // Spec step 4-5: error the writable with a TypeError and unblock any write
    // waiting on backpressure (`TransformStreamErrorWritableAndUnblockWrite`).
    let err = ctx
        .create_error_object(zuqe::ErrorKind::Type, "TransformStream terminated", false)
        .map(JSValue::from)
        .unwrap_or(JSValue::UNDEFINED);

    if let Some(stream_handle) = p.stream {
        let stream_val = JSValue::from(stream_handle);
        super::transform::transform_stream_error_writable_and_unblock_write(ctx, &stream_val, err)?;
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
        if let Some(s) = rcp.stream()
            && let Some(sp) = s.payload()
        {
            match sp.state {
                // Readable streams never enter the Erroring state.
                RsState::Errored | RsState::Erroring => return Ok(JSValue::NULL),
                RsState::Closed => return Ok(JSValue::ZERO),
                RsState::Readable => {}
            }
        }
        let total = rcp.queue_total_size();
        return Ok(JSValue::from(rcp.high_water_mark() - total));
    }
    Ok(JSValue::ZERO)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStreamDefaultController::register(rt)
}
