// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::RefCell, collections::HashMap};

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object, gc_lite::GcTrace,
};

use crate::{USER_CLASS_DEFS, aike::signal_number};

#[derive(Debug)]
pub(crate) struct EmitterPayload {
    /// event_name -> list of (is_once, callback)
    listeners: RefCell<HashMap<String, Vec<(bool, Object)>>>,

    /// Callbacks currently being dispatched by emit(). Moved out of
    /// `listeners` and parked here so gc_trace still reaches them while they
    /// run JS (which may trigger GC). Nested emits append after the current
    /// range and split their part off before returning, so it stays flat.
    dispatching: RefCell<Vec<(bool, Object)>>,
}

impl EmitterPayload {
    /// Create an empty payload (used for `process` and other pre-constructed emitters).
    pub(crate) fn new() -> Self {
        Self {
            listeners: RefCell::new(HashMap::new()),
            dispatching: RefCell::new(Vec::new()),
        }
    }

    fn gc_trace(&self, gcx: &mut zuqe::gc_lite::GcTraceCtx) {
        // try_borrow: if emit is in progress (mut borrowed), skip this GC cycle
        if let Ok(listeners) = self.listeners.try_borrow() {
            for list in listeners.values() {
                for (_, cb) in list {
                    cb.trace(gcx);
                }
            }
        }
        if let Ok(list) = self.dispatching.try_borrow() {
            for (_, cb) in list.iter() {
                cb.trace(gcx);
            }
        }
    }
}

fn get_emitter<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a EmitterPayload, i32> {
    JSEventEmitter::payload_of(val)
        .ok_or_else(|| ctx.throw_internal_error("not an EventEmitter instance"))
}

/// Convert event name string to a name owned String.
fn event_name(ctx: &mut ScopedContext, val: &JSValue) -> Result<String, i32> {
    let s = val.to_string(ctx)?;
    Ok(s.content().encode_utf8(false).to_string())
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    JSEventEmitter::new_instance(ctx, cc.this(), EmitterPayload::new()).map(JSValue::from)
}

/// addListener(event, listener) / on(event, listener)
fn js_add_listener(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_once = cc.magic() == 1;
    let this = cc.this();

    let listener = cc.arg(1).ensure_callable(ctx)?;
    let name = event_name(ctx, &cc.arg(0))?;

    // Signal events must use Aike.onSignal instead
    if signal_number(&name).is_some() {
        return Err(ctx.throw_type_error(&format!(
            "cannot listen for signal '{name}' on EventEmitter; use Aike.onSignal() instead"
        )));
    }

    let emitter = get_emitter(ctx, &this)?;

    // Regular events: stored in per-instance listeners map.
    emitter
        .listeners
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
    if let Ok(listener) = cc.arg(1).ensure_object(ctx) {
        // Regular event: remove from per-instance listeners map.
        let mut listeners = data.listeners.borrow_mut();
        if let Some(list) = listeners.get_mut(&name) {
            list.retain(|(_, cb)| listener.ne(cb));
            if list.is_empty() {
                listeners.remove(&name);
            }
        }
    }

    Ok(this)
}

/// emit(event, ...args)
fn js_emit(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let emitter = get_emitter(ctx, &this)?;

    let name = event_name(ctx, &cc.arg(0))?;
    if signal_number(&name).is_some() {
        return Err(ctx.throw_type_error(&format!(
            "cannot emit system signal '{name}' via EventEmitter; use Aike.onSignal() instead"
        )));
    }

    // Snapshot semantics: take the list out of the map (mutations during
    // dispatch don't affect the current emit), but park it on `dispatching`
    // so gc_trace keeps the callbacks alive — a GC inside cb.call() would
    // otherwise collect them (gc-lite "bad node" panic).
    let Some(mut lst) = emitter.listeners.borrow_mut().remove(&name) else {
        return Ok(JSValue::FALSE);
    };
    debug_assert!(!lst.is_empty());

    // Collect args (skip eventName)
    let args = if cc.length() > 1 {
        &cc.arguments()[1..]
    } else {
        &[]
    };

    // Our range is [start, start+n); nested emits only touch the tail after it.
    let n = lst.len();
    let start = {
        let mut disp = emitter.dispatching.borrow_mut();
        let start = disp.len();
        disp.append(&mut lst);
        start
    };

    for i in start..start + n {
        let (_, cb) = emitter.dispatching.borrow()[i].clone();
        if cb.call(ctx, JSValue::UNDEFINED, args).is_err() {
            let _e = ctx.take_exception();
            #[cfg(debug_assertions)]
            eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e); // Ignore listener errors
        }
    }

    let mut lst = emitter.dispatching.borrow_mut().split_off(start);
    // put non-once callbacks back; keep listeners added during dispatch
    lst.retain(|(once, _)| !*once);
    if !lst.is_empty() {
        let mut listeners = emitter.listeners.borrow_mut();
        let slot = listeners.entry(name).or_default();
        lst.append(slot); // survivors first, then ones added during dispatch
        *slot = lst;
    }

    Ok(JSValue::TRUE)
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

    let mut lst = Vec::<JSValue>::new();
    if let Some(list) = data.listeners.borrow().get(&name) {
        for (_, cb) in list {
            lst.push((*cb).into());
        }
    }

    Array::from_values(ctx, lst).map(JSValue::from)
}

/// listenerCount(event)
fn js_listener_count(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let emitter = get_emitter(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    let count = emitter.listeners.borrow().get(&name).map_or(0, |l| l.len());
    Ok(JSValue::from(count as i32))
}

/// eventNames() — returns all registered event names
fn js_event_names(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter(ctx, &this)?;

    let mut lst = Vec::<JSValue>::new();
    for key in data.listeners.borrow().keys() {
        let s = ctx.create_string_from_utf8(key.as_bytes().to_vec())?;
        lst.push(JSValue::make_string(s));
    }

    Array::from_values(ctx, lst).map(JSValue::from)
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
    Some(ClassCtorDef::new(js_constructor, 0, true).expose_to_global()),
    on_payload_gc: EmitterPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_event_emitter(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSEventEmitter::register(rt)
}
