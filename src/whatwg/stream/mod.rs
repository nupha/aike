// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Streams standard implementation.
//!
//! Implements ReadableStream, WritableStream, TransformStream and related
//! classes per the [WHATWG Streams specification](https://streams.spec.whatwg.org/).
//!
//! Split by class family: `readable`, `writable`, `transform`. Shared items
//! (stream state, queue entry, pending-read slot, queuing strategies) live here.

mod readable;
mod transform;
mod writable;

pub(crate) use {
    readable::{
        JSReadableStream, JSReadableStreamDefaultController, JSReadableStreamDefaultReader,
        readable_stream_from_bytes, readable_stream_from_channel,
    },
    transform::{JSTransformStream, JSTransformStreamDefaultController},
    writable::{
        JSWritableStream, JSWritableStreamDefaultController, JSWritableStreamDefaultWriter,
    },
};

use std::cell::RefCell;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum RsState {
    Readable,
    Closed,
    Errored,
}

pub(crate) struct QueueEntry {
    pub(crate) chunk: JSValue,
    pub(crate) size: f64,
}

thread_local! {
    /// Single in-flight read slot shared by reader/writer/transform paths.
    pub(crate) static PENDING_READ: RefCell<Option<(Promise, [JSValue; 2])>> =
        const { RefCell::new(None) };
}

/// Called by the runtime GC to keep PENDING_READ's JSValues alive.
pub(crate) fn gc_trace_stream(gcx: &mut GcTraceCtx) {
    PENDING_READ.with_borrow(|opt| {
        if let Some((promise, rr)) = opt.as_ref() {
            promise.trace(gcx);
            rr[0].trace(gcx);
            rr[1].trace(gcx);
        }
    });

    readable::CHANNEL_STREAMS.with_borrow(|map| {
        for (controller, _) in map.values() {
            controller.as_object().trace(gcx);
        }
    });
}

// ============================================================================
// QueuingStrategy payload
// ============================================================================

pub(crate) struct QueuingStrategyPayload;

// ============================================================================
// ByteLengthQueuingStrategy
// ============================================================================

const BYTE_LENGTH_QUEUING_STRATEGY_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("size", 1, js_byte_length_size, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ByteLengthQueuingStrategy",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "ByteLengthQueuingStrategy",
    JSByteLengthQueuingStrategy,
    QueuingStrategyPayload,
    BYTE_LENGTH_QUEUING_STRATEGY_PROTO,
    Some(ClassCtorDef::new(js_byte_length_constructor, 1, false).expose_to_global())
);

fn js_byte_length_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let init_val = cc.arg(0);
    let mut instance =
        JSByteLengthQueuingStrategy::new_instance(ctx, new_target, QueuingStrategyPayload)?;
    if let Some(obj) = init_val.as_object()
        && let Ok(hwm) = obj.get_property_str(ctx, "highWaterMark")
    {
        instance.define_property_value_str(ctx, "highWaterMark", hwm, zuqe::JS_PROP_C_W_E)?;
    }
    Ok(instance.into())
}

// ============================================================================
// CountQueuingStrategy
// ============================================================================

const COUNT_QUEUING_STRATEGY_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("size", 1, js_count_size, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "CountQueuingStrategy",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "CountQueuingStrategy",
    JSCountQueuingStrategy,
    QueuingStrategyPayload,
    COUNT_QUEUING_STRATEGY_PROTO,
    Some(ClassCtorDef::new(js_count_constructor, 1, false).expose_to_global())
);

fn js_count_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let init_val = cc.arg(0);
    let mut instance =
        JSCountQueuingStrategy::new_instance(ctx, new_target, QueuingStrategyPayload)?;
    if let Some(obj) = init_val.as_object()
        && let Ok(hwm) = obj.get_property_str(ctx, "highWaterMark")
    {
        instance.define_property_value_str(ctx, "highWaterMark", hwm, zuqe::JS_PROP_C_W_E)?;
    }
    Ok(instance.into())
}

fn js_byte_length_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.arg(0);
    if let Some(s) = chunk.as_string(false) {
        return Ok(JSValue::from(s.len() as f64));
    }
    if let Ok(arr) = TypedArray::try_from(chunk) {
        return Ok(JSValue::from(arr.byte_length() as i64));
    }

    let len = chunk
        .get_property_str(ctx, "byteLength")
        .or_else(|_| chunk.get_property_str(ctx, "length"))
        .unwrap_or(JSValue::from(0.0));

    Ok(JSValue::from(
        len.to_number(ctx).unwrap_or(zuqe::IntOrFloat::Int64(0)),
    ))
}

fn js_count_size(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Ok(JSValue::from(1.0))
}

// ============================================================================
// Helpers shared by readable/writable/transform
// ============================================================================

pub(crate) fn make_iterator_result(
    ctx: &mut ScopedContext,
    value: JSValue,
    done: bool,
) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;
    obj.define_property_value_str(ctx, "value", value, zuqe::JS_PROP_C_W_E)?;
    obj.define_property_value_str(ctx, "done", JSValue::from(done), zuqe::JS_PROP_C_W_E)?;
    Ok(JSValue::from(obj))
}

pub(crate) fn make_resolved_promise(
    ctx: &mut ScopedContext,
    value: JSValue,
) -> Result<JSValue, i32> {
    let (promise, rr) = Promise::create(ctx)?;
    rr[0].call(ctx, JSValue::UNDEFINED, &[value])?;
    Ok(promise.into())
}

pub(crate) fn make_rejected_promise(
    ctx: &mut ScopedContext,
    reason: JSValue,
) -> Result<JSValue, i32> {
    let (promise, rr) = Promise::create(ctx)?;
    rr[1].call(ctx, JSValue::UNDEFINED, &[reason])?;
    Ok(promise.into())
}

// ============================================================================
// Registration
// ============================================================================

#[distributed_slice(USER_CLASS_DEFS)]
fn register_byte_length_queuing_strategy(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSByteLengthQueuingStrategy::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_count_queuing_strategy(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSCountQueuingStrategy::register(rt)
}
