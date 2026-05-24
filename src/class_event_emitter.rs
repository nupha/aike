// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
};

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JS_PROP_C_W_E, JSResult, JSValue,
    NativeObject, NativePayload, Object, Runtime, ScopedContext, def_native_object,
    gc_lite::GcTrace,
};

use crate::USER_CLASS_DEFS;

#[derive(Debug)]
pub(crate) struct EmitterPayload {
    /// event_name -> list of (is_once, callback)
    listeners: RefCell<HashMap<String, Vec<(bool, JSValue)>>>,
}

impl EmitterPayload {
    fn gc_trace(&self, gcx: &mut zuqe::gc_lite::GcTraceCtx) {
        // try_borrow: if emit is in progress (mut borrowed), skip this GC cycle
        if let Ok(listeners) = self.listeners.try_borrow() {
            for list in listeners.values() {
                for (_, cb) in list {
                    cb.trace(gcx);
                }
            }
        }
    }
}

fn get_emitter<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a EmitterPayload, i32> {
    val.as_native_object::<EmitterPayload, JSEventEmitter>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_internal_error("not an EventEmitter instance");
            -1
        })
}

/// Convert event name string to a name owned String.
fn event_name(ctx: &mut ScopedContext, val: &JSValue) -> Result<String, i32> {
    let s = val.to_string(ctx)?;
    Ok(s.content().encode_utf8(false).to_string())
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let class_id = JSEventEmitter::class_id();
    let new_target = cc.this();

    let data = Box::new(EmitterPayload {
        listeners: RefCell::new(HashMap::new()),
    });
    let opaque = Box::into_raw(data).cast::<c_void>();

    ctx.create_class_instance_with_opaque(new_target, class_id, opaque)
        .map_err(|_| {
            let _ = unsafe { Box::from_raw(opaque.cast::<EmitterPayload>()) };
            -1
        })
        .map(JSValue::from)
}

/// addListener(event, listener) / on(event, listener)
fn js_add_listener(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_once = cc.magic() == 1;
    let this = cc.this();

    let listener = cc.arg(1);
    listener.ensure_function(ctx)?;

    let name = event_name(ctx, &cc.arg(0))?;

    // Signal events must use Aike.onSignal instead
    if crate::aike::signal::signal_number(&name).is_some() {
        ctx.throw_type_error(&format!(
            "cannot listen for signal '{name}' on EventEmitter; use Aike.onSignal() instead"
        ));
        return Err(-1);
    }

    let data = get_emitter(ctx, &this)?;

    // Regular events: stored in per-instance listeners map.
    data.listeners
        .borrow_mut()
        .entry(name)
        .or_default()
        .push((is_once, listener));

    Ok(this)
}

/// removeListener(event, listener) / off(event, listener)
fn js_remove_listener(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;

    let name = event_name(ctx, &cc.arg(0))?;
    let listener = cc.arg(1);

    // Regular event: remove from per-instance listeners map.
    let mut listeners = data.listeners.borrow_mut();
    if let Some(list) = listeners.get_mut(&name) {
        list.retain(|(_, cb)| *cb != listener);
        if list.is_empty() {
            listeners.remove(&name);
        }
    }

    Ok(this)
}

/// emit(event, ...args)
fn js_emit(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    // Cannot emit system signals via EventEmitter
    if crate::aike::signal::signal_number(&name).is_some() {
        ctx.throw_type_error(&format!(
            "cannot emit system signal '{name}' via EventEmitter; use Aike.onSignal() instead"
        ));
        return Err(-1);
    }

    // Collect args (skip event name)
    let mut args: Vec<JSValue> = Vec::with_capacity(cc.length().saturating_sub(1));
    for i in 1..cc.length() {
        args.push(cc.arg(i));
    }

    let mut fired = false;

    // Regular event: read from per-instance listeners map.
    // Clone first so borrow is released before JS execution (GC may run).
    let callbacks: Vec<(bool, JSValue)> = {
        let listeners = data.listeners.borrow();
        listeners.get(&name).map(|l| l.clone()).unwrap_or_default()
    };

    let mut to_remove: Vec<usize> = Vec::new();
    for (i, (is_once, cb)) in callbacks.iter().enumerate() {
        if cb.call(ctx, JSValue::UNDEFINED, &args).is_err() {
            // Ignore listener errors per Node.js convention
            let _ = ctx.rt().exception();
        }
        fired = true;
        if *is_once {
            to_remove.push(i);
        }
    }

    // Remove once-listeners in reverse index order
    if !to_remove.is_empty() {
        let mut list = data.listeners.borrow_mut();
        if let Some(l) = list.get_mut(&name) {
            to_remove.sort_unstable_by(|a, b| b.cmp(a));
            for i in to_remove {
                l.remove(i);
            }
            if l.is_empty() {
                list.remove(&name);
            }
        }
    }

    Ok(JSValue::Bool(fired))
}

/// removeAllListeners([event])
fn js_remove_all_listeners(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;

    if cc.length() > 0 && !cc.arg(0).is_undefined() {
        let name = event_name(ctx, &cc.arg(0))?;
        data.listeners.borrow_mut().remove(&name);
    } else {
        data.listeners.borrow_mut().clear();
    }

    Ok(this)
}

/// listeners(event) — returns callbacks for a specific event
fn js_listeners(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    let mut arr = Array::create(ctx)?;
    let mut idx = 0u32;

    if let Some(list) = data.listeners.borrow().get(&name) {
        for (_, cb) in list {
            arr.define_property_value_u32(ctx, idx, *cb, JS_PROP_C_W_E)?;
            idx += 1;
        }
    }

    Ok(arr.into())
}

/// listenerCount(event)
fn js_listener_count(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    let count = data.listeners.borrow().get(&name).map_or(0, |l| l.len());

    Ok(JSValue::from(count as i32))
}

/// eventNames() — returns all registered event names
fn js_event_names(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;
    let mut arr = Array::create(ctx)?;
    let mut idx = 0u32;

    for key in data.listeners.borrow().keys() {
        let s = ctx.create_string_from_utf8(key.as_bytes().to_vec())?;
        arr.define_property_value_u32(ctx, idx, s.into(), JS_PROP_C_W_E)?;
        idx += 1;
    }

    Ok(arr.into())
}

const CLASS_EVENT_EMITTER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("addListener", 2, js_add_listener, 0),
    FieldDef::FUNC("on", 2, js_add_listener, 0),
    FieldDef::FUNC("once", 2, js_add_listener, 1),
    FieldDef::FUNC("removeListener", 2, js_remove_listener, 0),
    FieldDef::FUNC("off", 2, js_remove_listener, 0),
    FieldDef::FUNC("emit", 1, js_emit, 0), // actually variadic
    FieldDef::FUNC("removeAllListeners", 0, js_remove_all_listeners, 0),
    FieldDef::FUNC("listeners", 1, js_listeners, 0),
    FieldDef::FUNC("listenerCount", 1, js_listener_count, 0),
    FieldDef::FUNC("eventNames", 0, js_event_names, 0),
];

def_native_object!(
    "EventEmitter",
    JSEventEmitter,
    EmitterPayload,
    CLASS_EVENT_EMITTER_PROTO,
    constructor: ClassCtorDef::new(js_constructor, 0, true).expose_to_global(),
    on_payload_gc: EmitterPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_event_emitter(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSEventEmitter::register(rt)
}
