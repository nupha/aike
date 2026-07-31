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

/// Resolve the single in-flight `PENDING_READ` with an iterator result
/// `{value, done}`. Returns true if a pending read was present and resolved.
pub(super) fn resolve_pending_read(ctx: &mut ScopedContext, value: JSValue, done: bool) -> bool {
    PENDING_READ.with(|slot| {
        let mut s = slot.borrow_mut();
        if let Some((_promise, rr)) = s.take() {
            if let Ok(result) = make_iterator_result(ctx, value, done) {
                let _ = rr[0].call(ctx, JSValue::UNDEFINED, &[result]);
                true
            } else {
                false
            }
        } else {
            false
        }
    })
}

/// Reject the single in-flight `PENDING_READ`, if any.
pub(super) fn reject_pending_read(ctx: &mut ScopedContext, reason: JSValue) {
    PENDING_READ.with(|slot| {
        let mut s = slot.borrow_mut();
        if let Some((mut promise, _slot)) = s.take() {
            promise.reject(ctx, reason);
        }
    });
}

/// Call an optionally-present, callable method `name` on `obj`.
///
/// `this` is bound to `obj` (all call sites pass the receiver as `this`).
/// Returns `Some(call result)` if the method existed and was invoked,
/// `None` if `obj` is not an object / the property was absent / not callable.
///
/// Note: implemented via the public `get_property_str` + `is_callable` API.
/// zuqe's `Object::invoke_callable`/`invoke_defined` do the same but take an
/// `Atom`, and aike's public API has no `&str -> Atom` constructor
/// (`Atom::from_str` / `StringImpl::to_atom` are `pub(crate)`), so the
/// string-based form is the idiomatic aike-level equivalent.
pub(super) fn call_opt_method(
    ctx: &mut ScopedContext,
    obj: JSValue,
    name: &str,
    args: &[JSValue],
) -> Result<Option<JSValue>, i32> {
    let Some(o) = obj.as_object() else {
        return Ok(None);
    };
    let Ok(f) = o.get_property_str(ctx, name) else {
        return Ok(None);
    };
    if !f.is_callable() {
        return Ok(None);
    }
    f.call(ctx, obj, args).map(Some)
}

/// Set a stream's state (and `stored_error` when errored), then settle the
/// peer reader/writer's `closed_promise` accordingly.
///
/// `reason`: `None` => `closed_promise.resolve(undefined)`; `Some(r)` =>
/// `closed_promise.reject(r)` and `stream.stored_error = r`.
///
/// `stream` must be a `JSReadableStream` or `JSWritableStream` native object.
/// Taken by value (Copy) and re-derived inside; the caller keeps its own handle.
pub(super) fn settle_stream_state(
    ctx: &mut ScopedContext,
    stream: JSValue,
    state: RsState,
    reason: Option<JSValue>,
) {
    let mut stream = stream;
    if let Some(sp) = JSReadableStream::payload_mut_of(&mut stream) {
        // A stream can only leave the Readable state once. Re-settling (e.g.
        // cancel after an error) must not re-settle the peer's closed_promise,
        // which would violate the Promise contract.
        if sp.state != RsState::Readable {
            return;
        }
        sp.state = state;
        if let Some(r) = reason {
            sp.stored_error = r;
        }
        notify_peer_closed(ctx, sp.reader.map(JSValue::from), reason);
    } else if let Some(sp) = JSWritableStream::payload_mut_of(&mut stream) {
        if sp.state != RsState::Readable {
            return;
        }
        sp.state = state;
        if let Some(r) = reason {
            sp.stored_error = r;
        }
        notify_peer_closed(ctx, sp.writer.map(JSValue::from), reason);
    }
}

/// Resolve or reject the peer (reader/writer) `closed_promise` if present.
///
/// The `closed_promise` is taken out (`Option::take`) so it can only be settled
/// once: settling it twice would violate Promise's contract (a settled promise
/// must ignore further resolve/reject). Fetch body teardown can call
/// `settle_stream_state` repeatedly (error then cancel) against the same reader.
fn notify_peer_closed(ctx: &mut ScopedContext, peer: Option<JSValue>, reason: Option<JSValue>) {
    let Some(mut pv) = peer else {
        return;
    };

    let mut settle = |cp: &mut Promise| {
        if let Some(r) = reason {
            cp.reject(ctx, r);
        } else {
            cp.resolve(ctx, JSValue::UNDEFINED);
        }
    };

    if let Some(rp) = JSReadableStreamDefaultReader::payload_mut_of(&mut pv) {
        if let Some(mut cp) = rp.closed_promise.take() {
            settle(&mut cp);
        }
    } else if let Some(wp) = JSWritableStreamDefaultWriter::payload_mut_of(&mut pv) {
        if let Some(mut cp) = wp.closed_promise.take() {
            settle(&mut cp);
        }
    }
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
