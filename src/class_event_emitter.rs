// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    sync::atomic::AtomicUsize,
};

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassDef, ClassID, FieldDef, JS_PROP_C_W_E, JSResult,
    JSValue, Object, Runtime, RuntimeImpl, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

// ---------------------------------------------------------------------------
// Global signal dispatch table
// Maps signal number -> list of (instance_id, callback).
// Multiple EventEmitter instances can listen to the same signal.
// ---------------------------------------------------------------------------

thread_local! {
    pub(super) static OS_PENDING_SIGNALS: Cell<u64> = const { Cell::new(0) };

    static SIGNAL_DISPATCH: RefCell<HashMap<u32, Vec<(usize, JSValue)>>> =
        RefCell::new(HashMap::new());

    // Bitmask: which signals have their C handler installed already
    static INSTALLED_SIGNALS: Cell<u64> = const { Cell::new(0) };
}

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

// Per-instance opaque data
struct EmitterData {
    /// event_name -> list of (is_once, callback)
    listeners: RefCell<HashMap<String, Vec<(bool, JSValue)>>>,
    /// unique ID used to associate signal dispatch entries with this instance
    instance_id: usize,
    /// signal numbers we've registered for (for cleanup on dispose)
    tracked_signals: RefCell<Vec<u32>>,
}

impl EmitterData {
    fn install_signal_handler(&self, sig_num: u32) {
        INSTALLED_SIGNALS.with(|installed| {
            let mask = 1_u64 << sig_num;
            if installed.get() & mask == 0 {
                installed.set(installed.get() | mask);

                extern "C" fn signal_handler(sig_num: i32) {
                    OS_PENDING_SIGNALS.with(|s| s.set(s.get() | (1_u64 << sig_num)));
                }
                unsafe {
                    libc::signal(sig_num as _, signal_handler as *const c_void as usize);
                }
            }
        });
    }
}

pub(crate) fn gc_trace_emitter(tr: &mut GcTraceCtx) {
    SIGNAL_DISPATCH.with_borrow_mut(|dispatch| {
        for list in dispatch.values_mut() {
            for (_, cb) in list.iter_mut() {
                cb.trace(tr);
            }
        }
    });
}

/// Poll function: checks OS_PENDING_SIGNALS and dispatches to EventEmitter
/// signal listeners.  Installed as a tick handler.
pub(crate) fn poll_emitter_signals(ctx: &mut ScopedContext) -> bool {
    let pending = OS_PENDING_SIGNALS.get();
    if pending == 0 {
        return false;
    }

    let mut callbacks = smallvec::SmallVec::<[JSValue; 16]>::new();

    SIGNAL_DISPATCH.with(|lut| {
        for (sig_num, listeners) in lut.borrow().iter() {
            let mask = 1_u64 << *sig_num as u64;
            if pending & mask != 0 {
                for (_, cb) in listeners {
                    callbacks.push(*cb);
                }
            }
        }
    });

    if callbacks.is_empty() {
        return false;
    }

    // Clear handled signal bits
    let mut cleared = pending;
    SIGNAL_DISPATCH.with(|lut| {
        for sig_num in lut.borrow().keys() {
            cleared &= !(1_u64 << *sig_num as u64);
        }
    });
    OS_PENDING_SIGNALS.set(cleared);

    for cb in callbacks {
        if cb.call(ctx, JSValue::UNDEFINED, &[]).is_err() {
            let msg = ctx.rt().exception().to_text(ctx);
            eprintln!("EventEmitter signal handler error: {}", msg.message);
        }
    }

    true
}

/// retrive EmitterData from object
fn get_emitter_data<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<&'a EmitterData, i32> {
    if let Some(o) = val.as_object() {
        let payload = o
            .retrive_payload(ctx, o.class_id())?
            .other()
            .ok_or_else(|| {
                ctx.throw_internal_error("not a EventEmitter instance");
                -1
            })?;
        Ok(unsafe { &*payload.opaque().cast::<EmitterData>() })
    } else {
        ctx.throw_internal_error("not a EventEmitter instance");
        Err(-1)
    }
}

/// Convert event name string to a name owned String.
fn event_name(ctx: &mut ScopedContext, val: &JSValue) -> Result<String, i32> {
    let s = val.to_string(ctx)?;
    Ok(s.content().encode_utf8(false).to_string())
}

/// Return the signal number for a signal name, or None.
fn signal_number(name: &str) -> Option<i32> {
    Some(match name {
        "SIGINT" => libc::SIGINT,
        "SIGABRT" => libc::SIGABRT,
        "SIGFPE" => libc::SIGFPE,
        "SIGILL" => libc::SIGILL,
        "SIGSEGV" => libc::SIGSEGV,
        "SIGTERM" => libc::SIGTERM,
        #[cfg(not(windows))]
        "SIGQUIT" => libc::SIGQUIT,
        #[cfg(not(windows))]
        "SIGPIPE" => libc::SIGPIPE,
        #[cfg(not(windows))]
        "SIGALRM" => libc::SIGALRM,
        #[cfg(not(windows))]
        "SIGUSR1" => libc::SIGUSR1,
        #[cfg(not(windows))]
        "SIGUSR2" => libc::SIGUSR2,
        #[cfg(not(windows))]
        "SIGCHLD" => libc::SIGCHLD,
        #[cfg(not(windows))]
        "SIGCONT" => libc::SIGCONT,
        #[cfg(not(windows))]
        "SIGSTOP" => libc::SIGSTOP,
        #[cfg(not(windows))]
        "SIGTSTP" => libc::SIGTSTP,
        #[cfg(not(windows))]
        "SIGTTIN" => libc::SIGTTIN,
        #[cfg(not(windows))]
        "SIGTTOU" => libc::SIGTTOU,
        _ => return None,
    })
    .filter(|n| *n >= 0 && *n < 64)
}

/// Reverse of signal_number: convert a signal number back to its name.
fn sig_name(sig: u32) -> Option<&'static str> {
    Some(match sig as i32 {
        n if n == libc::SIGINT => "SIGINT",
        n if n == libc::SIGABRT => "SIGABRT",
        n if n == libc::SIGFPE => "SIGFPE",
        n if n == libc::SIGILL => "SIGILL",
        n if n == libc::SIGSEGV => "SIGSEGV",
        n if n == libc::SIGTERM => "SIGTERM",
        #[cfg(not(windows))]
        n if n == libc::SIGQUIT => "SIGQUIT",
        #[cfg(not(windows))]
        n if n == libc::SIGPIPE => "SIGPIPE",
        #[cfg(not(windows))]
        n if n == libc::SIGALRM => "SIGALRM",
        #[cfg(not(windows))]
        n if n == libc::SIGUSR1 => "SIGUSR1",
        #[cfg(not(windows))]
        n if n == libc::SIGUSR2 => "SIGUSR2",
        #[cfg(not(windows))]
        n if n == libc::SIGCHLD => "SIGCHLD",
        #[cfg(not(windows))]
        n if n == libc::SIGCONT => "SIGCONT",
        #[cfg(not(windows))]
        n if n == libc::SIGSTOP => "SIGSTOP",
        #[cfg(not(windows))]
        n if n == libc::SIGTSTP => "SIGTSTP",
        #[cfg(not(windows))]
        n if n == libc::SIGTTIN => "SIGTTIN",
        #[cfg(not(windows))]
        n if n == libc::SIGTTOU => "SIGTTOU",
        _ => return None,
    })
}

fn js_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let class_id = JSEventEmitter::class_id();
    let new_target = cc.this();

    let data = Box::new(EmitterData {
        listeners: RefCell::new(HashMap::new()),
        instance_id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        tracked_signals: RefCell::new(Vec::new()),
    });
    let opaque = Box::into_raw(data).cast::<c_void>();

    ctx.create_class_instance_with_opaque(new_target, class_id, opaque)
        .map_err(|_| {
            let _ = unsafe { Box::from_raw(opaque.cast::<EmitterData>()) };
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
    let data = get_emitter_data(ctx, &this)?;

    if let Some(sig) = signal_number(&name) {
        // Signal events: stored only in SIGNAL_DISPATCH (not in listeners).
        // OS signals are dispatched via poll_emitter_signals; manual emit("SIGXXX")
        // reads SIGNAL_DISPATCH too, so a single source of truth is enough.
        data.install_signal_handler(sig as u32);
        SIGNAL_DISPATCH.with(|lut| {
            lut.borrow_mut()
                .entry(sig as u32)
                .or_default()
                .push((data.instance_id, listener));
        });
        data.tracked_signals.borrow_mut().push(sig as u32);
    } else {
        // Regular events: stored in per-instance listeners map.
        data.listeners
            .borrow_mut()
            .entry(name)
            .or_default()
            .push((is_once, listener));
    }

    Ok(this)
}

/// removeListener(event, listener) / off(event, listener)
fn js_remove_listener(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter_data(ctx, &this)?;

    let name = event_name(ctx, &cc.arg(0))?;
    let listener = cc.arg(1);

    if let Some(sig) = signal_number(&name) {
        // Signal: remove from SIGNAL_DISPATCH only (not stored in listeners).
        SIGNAL_DISPATCH.with(|lut| {
            let mut d = lut.borrow_mut();
            if let Some(list) = d.get_mut(&(sig as u32)) {
                list.retain(|(id, cb)| *id != data.instance_id || *cb != listener);
                if list.is_empty() {
                    d.remove(&(sig as u32));
                }
            }
        });
        // Remove one occurrence from tracked_signals (Vec may have duplicates
        // when the same signal is registered multiple times).
        let mut tracked = data.tracked_signals.borrow_mut();
        if let Some(pos) = tracked.iter().position(|s| *s == sig as u32) {
            tracked.swap_remove(pos);
        }
    } else {
        // Regular event: remove from per-instance listeners map.
        let mut listeners = data.listeners.borrow_mut();
        if let Some(list) = listeners.get_mut(&name) {
            list.retain(|(_, cb)| *cb != listener);
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
    let data = get_emitter_data(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    // Collect args (skip event name)
    let mut args: Vec<JSValue> = Vec::with_capacity(cc.length().saturating_sub(1));
    for i in 1..cc.length() {
        args.push(cc.arg(i));
    }

    let mut fired = false;

    if let Some(sig) = signal_number(&name) {
        // Signal emit: read callbacks from SIGNAL_DISPATCH for this instance.
        // Clone to release borrow before calling into JS.
        let callbacks: Vec<JSValue> = SIGNAL_DISPATCH.with(|lut| {
            lut.borrow()
                .get(&(sig as u32))
                .map(|list| {
                    list.iter()
                        .filter(|(id, _)| *id == data.instance_id)
                        .map(|(_, cb)| *cb)
                        .collect()
                })
                .unwrap_or_default()
        });
        for cb in &callbacks {
            if cb.call(ctx, JSValue::UNDEFINED, &args).is_err() {
                let _ = ctx.rt().exception();
            }
            fired = true;
        }
    } else {
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
    }

    Ok(JSValue::Bool(fired))
}

/// removeAllListeners([event])
fn js_remove_all_listeners(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter_data(ctx, &this)?;

    if cc.length() > 0 && !cc.arg(0).is_undefined() {
        let name = event_name(ctx, &cc.arg(0))?;

        if let Some(sig) = signal_number(&name) {
            // Signal: remove only this instance's entries, not other instances'.
            SIGNAL_DISPATCH.with(|lut| {
                let mut d = lut.borrow_mut();
                if let Some(list) = d.get_mut(&(sig as u32)) {
                    list.retain(|(id, _)| *id != data.instance_id);
                    if list.is_empty() {
                        d.remove(&(sig as u32));
                    }
                }
            });
            data.tracked_signals
                .borrow_mut()
                .retain(|s| *s != sig as u32);
        } else {
            data.listeners.borrow_mut().remove(&name);
        }
    } else {
        // Remove all listeners for this instance.
        let sigs = data.tracked_signals.borrow().clone();
        SIGNAL_DISPATCH.with(|lut| {
            let mut d = lut.borrow_mut();
            for sig in &sigs {
                if let Some(list) = d.get_mut(sig) {
                    list.retain(|(id, _)| *id != data.instance_id);
                    if list.is_empty() {
                        d.remove(sig);
                    }
                }
            }
        });
        data.tracked_signals.borrow_mut().clear();
        data.listeners.borrow_mut().clear();
    }

    Ok(this)
}

/// listeners(event) — returns callbacks for both regular and signal events
fn js_listeners(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter_data(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    let mut arr = Array::create(ctx)?;
    let mut idx = 0u32;

    if let Some(sig) = signal_number(&name) {
        // Signal: collect from SIGNAL_DISPATCH filtered by instance_id
        SIGNAL_DISPATCH.with(|lut| {
            if let Some(list) = lut.borrow().get(&(sig as u32)) {
                for (id, cb) in list {
                    if *id == data.instance_id {
                        let _ = arr.define_property_value_u32(ctx, idx, *cb, JS_PROP_C_W_E);
                        idx += 1;
                    }
                }
            }
        });
    } else {
        if let Some(list) = data.listeners.borrow().get(&name) {
            for (_, cb) in list {
                arr.define_property_value_u32(ctx, idx, *cb, JS_PROP_C_W_E)?;
                idx += 1;
            }
        }
    }

    Ok(arr.into())
}

/// listenerCount(event)
fn js_listener_count(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter_data(ctx, &this)?;
    let name = event_name(ctx, &cc.arg(0))?;

    let count = if let Some(sig) = signal_number(&name) {
        // Signal: count entries matching this instance
        SIGNAL_DISPATCH.with(|lut| {
            lut.borrow()
                .get(&(sig as u32))
                .map(|list| {
                    list.iter()
                        .filter(|(id, _)| *id == data.instance_id)
                        .count()
                })
                .unwrap_or(0)
        })
    } else {
        data.listeners.borrow().get(&name).map_or(0, |l| l.len())
    };

    Ok(JSValue::from(count as i32))
}

/// eventNames() — returns names from both regular listeners and signal dispatch
fn js_event_names(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let data = get_emitter_data(ctx, &this)?;
    let mut arr = Array::create(ctx)?;
    let mut idx = 0u32;

    // Regular events
    for key in data.listeners.borrow().keys() {
        let s = ctx.create_string_from_utf8(key.as_bytes().to_vec())?;
        arr.define_property_value_u32(ctx, idx, s.into(), JS_PROP_C_W_E)?;
        idx += 1;
    }

    // Signal events registered for this instance
    let tracked = data.tracked_signals.borrow().clone();
    SIGNAL_DISPATCH.with(|lut| {
        let d = lut.borrow();
        for sig in &tracked {
            // Only include if this instance still has entries
            let has = d
                .get(sig)
                .map(|list| list.iter().any(|(id, _)| *id == data.instance_id))
                .unwrap_or(false);
            if has {
                // Convert sig number back to name for the JS array
                if let Some(name) = sig_name(*sig) {
                    let s = ctx.create_string_from_utf8(name.as_bytes().to_vec());
                    if let Ok(s) = s {
                        let _ = arr.define_property_value_u32(ctx, idx, s.into(), JS_PROP_C_W_E);
                        idx += 1;
                    }
                }
            }
        }
    });

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

pub struct JSEventEmitter;

impl JSEventEmitter {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn handle_dispose(_: &mut RuntimeImpl, opaque: *mut c_void, _: usize) {
        if !opaque.is_null() {
            let data = unsafe { Box::from_raw(opaque.cast::<EmitterData>()) };

            // Remove signal dispatch entries for this instance
            SIGNAL_DISPATCH.with(|lut| {
                let mut d = lut.borrow_mut();
                for sig in data.tracked_signals.borrow().iter() {
                    if let Some(list) = d.get_mut(sig) {
                        list.retain(|(id, _)| *id != data.instance_id);
                        if list.is_empty() {
                            d.remove(sig);
                        }
                    }
                }
            });
        }
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt
            .register_class(
                ClassDef::new(
                    "EventEmitter",
                    CLASS_EVENT_EMITTER_PROTO,
                    Some(ClassCtorDef::new(js_constructor, 0, true).expose_to_global()),
                )
                .on_gc_trace(|opaque: *mut c_void, gcx: &mut zuqe::gc_lite::GcTraceCtx| {
                    if !opaque.is_null() {
                        let data = unsafe { &*(opaque.cast::<EmitterData>()) };
                        // try_borrow: if emit is in progress (mut borrowed), skip this GC cycle
                        if let Ok(listeners) = data.listeners.try_borrow() {
                            for list in listeners.values() {
                                for (_, cb) in list {
                                    cb.trace(gcx);
                                }
                            }
                        }
                    }
                })
                .on_dispose(Self::handle_dispose),
            )
            .expect("register class EventEmitter failed");

        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }
}

/// Create a standalone EventEmitter instance with its own EmitterData.
/// Useful when an object needs to behave as an EventEmitter (e.g. process).
pub(crate) fn create_event_emitter_instance(ctx: &mut ScopedContext) -> Result<Object, i32> {
    let data = Box::new(EmitterData {
        listeners: RefCell::new(HashMap::new()),
        instance_id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        tracked_signals: RefCell::new(Vec::new()),
    });
    let opaque = Box::into_raw(data).cast::<c_void>();

    ctx.create_class_instance_with_opaque(JSValue::UNDEFINED, JSEventEmitter::class_id(), opaque)
        .map_err(|_| {
            let _ = unsafe { Box::from_raw(opaque.cast::<EmitterData>()) };
            -1
        })
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_event_emitter(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSEventEmitter::register(rt)
}
