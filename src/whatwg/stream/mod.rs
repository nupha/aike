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
        readable_stream_from_bytes, readable_stream_from_channel, readable_stream_from_text,
        readable_stream_from_text_channel,
    },
    transform::{JSTransformStream, JSTransformStreamDefaultController},
    writable::{
        JSWritableStream, JSWritableStreamDefaultController, JSWritableStreamDefaultWriter,
    },
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::promise::Promise,
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

/// Called by the runtime GC to keep stream-related JSValues alive.
pub(crate) fn gc_trace_stream(gcx: &mut GcTraceCtx) {
    readable::CHANNEL_STREAMS.with_borrow(|map| {
        for (controller, _, _) in map.values() {
            controller.as_object().trace(gcx);
        }
    });
}

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
    (),
    BYTE_LENGTH_QUEUING_STRATEGY_PROTO,
    Some(ClassCtorDef::new(js_byte_length_constructor, 1, false).expose_to_global())
);

fn js_byte_length_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let init_val = cc.arg(0);
    let mut instance = JSByteLengthQueuingStrategy::new_instance(ctx, new_target)?;
    let obj = queuing_strategy_init(ctx, init_val)?;
    let hwm = obj.get_property_str(ctx, "highWaterMark")?;
    if hwm.is_undefined() {
        return Err(ctx.throw_type_error("highWaterMark is required"));
    }
    let num = hwm.to_number(ctx)?.to_f64();
    instance.define_property_value_str(
        ctx,
        "highWaterMark",
        JSValue::from(num),
        zuqe::JS_PROP_C_W_E,
    )?;
    Ok(instance.into())
}

/// WebIDL `QueuingStrategy` dict init: must be a (non-null) object.
/// Returns the init object, or throws TypeError for undefined/null/non-object.
fn queuing_strategy_init<'a>(
    ctx: &mut ScopedContext,
    init_val: JSValue,
) -> Result<zuqe::Object, i32> {
    if init_val.is_undefined() || init_val.is_null() || init_val.as_object().is_none() {
        return Err(ctx.throw_type_error("QueuingStrategy requires an object init"));
    }
    Ok(init_val.as_object().unwrap())
}

// ============================================================================
// CountQueuingStrategy
// ============================================================================

const COUNT_QUEUING_STRATEGY_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("size", 0, js_count_size, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "CountQueuingStrategy",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "CountQueuingStrategy",
    JSCountQueuingStrategy,
    (),
    COUNT_QUEUING_STRATEGY_PROTO,
    Some(ClassCtorDef::new(js_count_constructor, 1, false).expose_to_global())
);

fn js_count_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let init_val = cc.arg(0);
    let mut instance = JSCountQueuingStrategy::new_instance(ctx, new_target)?;
    let obj = queuing_strategy_init(ctx, init_val)?;
    let hwm = obj.get_property_str(ctx, "highWaterMark")?;
    if hwm.is_undefined() {
        return Err(ctx.throw_type_error("highWaterMark is required"));
    }
    let num = hwm.to_number(ctx)?.to_f64();
    instance.define_property_value_str(
        ctx,
        "highWaterMark",
        JSValue::from(num),
        zuqe::JS_PROP_C_W_E,
    )?;
    Ok(instance.into())
}

fn js_byte_length_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.arg(0);
    if chunk.is_undefined() || chunk.is_null() {
        return Err(ctx.throw_type_error("chunk is required"));
    }
    let Some(obj) = chunk.as_object() else {
        return Ok(JSValue::UNDEFINED);
    };
    let atom = zuqe::Atom::from_static(ctx, "byteLength")?;
    if !obj.has_property(ctx, atom)? {
        return Ok(JSValue::UNDEFINED);
    }
    obj.get_property(ctx, atom)
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
    // The iterator result `{value, done}` is an internal object. An injected
    // `Object.prototype.then` must not make it look like a thenable when the
    // reader's read promise is resolved with it (WPT forbids intercepting the
    // body conversion this way). Shadow `then` with a non-callable own property
    // so the promise resolution settles with the object directly.
    obj.define_property_value_str(ctx, "then", JSValue::UNDEFINED, zuqe::JS_PROP_C_W_E)?;
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
pub(crate) fn call_opt_method(
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
