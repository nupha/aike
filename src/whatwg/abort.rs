// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG DOM Standard — AbortController and AbortSignal implementations.
//!
//! Provides `AbortController` and `AbortSignal` for cooperative cancellation
//! of asynchronous operations, following the WHATWG spec.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use linkme::distributed_slice;
use tokio::sync::Notify;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, Exception, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, whatwg::dom::make_dom_exception};

// ===========================================================================
// Shared signal state (Rc<RefCell<>> so both Controller and Signal see updates)
// ===========================================================================

struct SignalInner {
    aborted: bool,
    reason: JSValue,
    // The associated AbortController (for follow signal to find controller)
    controller: Option<Object>,
    /// Notifies waiters (e.g. an in-flight fetch) when this signal aborts.
    notify: Arc<Notify>,
    /// A request body (e.g. a `ReadableStream`) that must be canceled with the
    /// abort reason when this signal aborts, per the Fetch spec. Set by `fetch`
    /// when it uploads a streaming body. GC-traced via the signal payload.
    on_abort_body: Option<JSValue>,
    /// Follower signals created via `Request`'s "follow" semantics. When this
    /// signal aborts, each follower is aborted with the same reason.
    followers: Vec<Rc<RefCell<SignalInner>>>,
    /// Event listeners registered via `addEventListener` (EventTarget subset).
    /// Only the `abort` event type is meaningful for AbortSignal.
    listeners: Vec<(String, JSValue)>,
}

// ===========================================================================
// AbortSignal
// ===========================================================================

pub(crate) struct AbortSignalPayload {
    inner: Rc<RefCell<SignalInner>>,
}

fn get_signal<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a AbortSignalPayload, i32> {
    JSAbortSignal::payload_of(val)
        .ok_or_else(|| ctx.throw_internal_error("not an AbortSignal instance"))
}

fn js_signal_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // Per spec: AbortSignal constructor is not directly callable from JS.
    // Users obtain signals via AbortController.signal or static methods.
    Err(ctx.throw_type_error(
        "Illegal constructor: use AbortController or AbortSignal.abort()/timeout()",
    ))
}

fn js_signal_aborted_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    Ok(JSValue::from(signal.inner.borrow().aborted))
}

fn js_signal_reason_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    let inner = signal.inner.borrow();
    if inner.aborted {
        Ok(inner.reason)
    } else {
        Ok(JSValue::UNDEFINED)
    }
}

fn js_signal_throw_if_aborted(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    let inner = signal.inner.borrow();
    if inner.aborted {
        // Throw the signal's abort reason (per spec, e.g. a DOMException
        // "AbortError" or whatever reason was passed to abort()).
        let reason = inner.reason;
        ctx.throw(Exception::Error(reason));
        return Err(-1);
    } else {
        Ok(JSValue::UNDEFINED)
    }
}

/// Fire all `abort` event listeners registered on `inner`. Callers must drop
/// any borrow of `inner` before calling this (the listener callbacks may
/// re-enter the signal).
fn fire_abort_events(ctx: &mut ScopedContext, inner: &Rc<RefCell<SignalInner>>) {
    let listeners: Vec<JSValue> = inner
        .borrow()
        .listeners
        .iter()
        .filter(|(t, _)| t == "abort")
        .map(|(_, cb)| *cb)
        .collect();
    if listeners.is_empty() {
        return;
    }
    let mut event = match ctx.create_base_object() {
        Ok(e) => e,
        Err(_) => return,
    };
    let type_val = JSValue::make_string(ctx.create_string_from_utf8(b"abort".to_vec()).unwrap());
    let _ = event.set_property_str(ctx, "type", type_val);
    let _ = event.set_property_str(ctx, "bubbles", JSValue::from(false));
    let _ = event.set_property_str(ctx, "cancelable", JSValue::from(false));

    for cb in listeners {
        if cb.is_callable() && cb.call(ctx, JSValue::UNDEFINED, &[event.into()]).is_err() {
            let _e = ctx.take_exception();
            #[cfg(debug_assertions)]
            eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
        }
    }
}

fn js_signal_add_event_listener(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    if cc.length() < 1 {
        return Ok(JSValue::UNDEFINED);
    }
    let event = cc.arg(0);
    let type_name = match event.to_string(ctx) {
        Ok(s) => s.content().encode_utf8(false).to_string(),
        Err(_) => return Ok(JSValue::UNDEFINED),
    };
    let cb = if cc.length() > 1 {
        cc.arg(1)
    } else {
        JSValue::UNDEFINED
    };
    if cb.is_callable() {
        signal.inner.borrow_mut().listeners.push((type_name, cb));
    }
    Ok(JSValue::UNDEFINED)
}

fn js_signal_remove_event_listener(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    if cc.length() < 2 {
        return Ok(JSValue::UNDEFINED);
    }
    let event = cc.arg(0);
    let type_name = match event.to_string(ctx) {
        Ok(s) => s.content().encode_utf8(false).to_string(),
        Err(_) => return Ok(JSValue::UNDEFINED),
    };
    let cb = cc.arg(1);
    signal
        .inner
        .borrow_mut()
        .listeners
        .retain(|(t, c)| !(t == &type_name && *c == cb));
    Ok(JSValue::UNDEFINED)
}

fn js_signal_dispatch_event(_: &mut ScopedContext, _: CallContext) -> JSResult {
    // AbortSignal only meaningfully dispatches `abort`; we don't support arbitrary
    // event dispatch here. Spec-compliant no-op for non-abort events.
    Ok(JSValue::TRUE)
}

fn js_signal_onabort_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    let inner = signal.inner.borrow();
    for (t, cb) in &inner.listeners {
        if t == "onabort" {
            return Ok(*cb);
        }
    }
    Ok(JSValue::UNDEFINED)
}

fn js_signal_onabort_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let signal = get_signal(ctx, &this)?;
    let cb = cc.arg(0);
    let mut inner = signal.inner.borrow_mut();
    inner.listeners.retain(|(t, _)| t != "onabort");
    if !cb.is_undefined_or_null() {
        inner.listeners.push(("onabort".to_string(), cb));
    }
    Ok(JSValue::UNDEFINED)
}

/// Static method: AbortSignal.abort(reason?)
fn js_signal_static_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // Spec: `AbortSignal.abort(reason = new DOMException('...', 'AbortError'))`.
    // An explicitly passed `undefined` still triggers the default.
    let reason = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0)
    } else {
        make_dom_exception(ctx, "AbortError", "The operation was aborted.")?
    };

    let inner = Rc::new(RefCell::new(SignalInner {
        aborted: true,
        reason,
        controller: None,
        notify: Arc::new(Notify::new()),
        on_abort_body: None,
        followers: Vec::new(),
        listeners: Vec::new(),
    }));
    inner.borrow().notify.notify_waiters();

    let signal =
        JSAbortSignal::new_instance(ctx, JSValue::UNDEFINED, AbortSignalPayload { inner })?;
    Ok(signal.into())
}

/// Static method: AbortSignal.timeout(delay)
///
/// TODO: The proper implementation requires scheduling a JS microtask.
/// For now, returns a signal that aborts immediately.
fn js_signal_static_timeout(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let _ = cc.length(); // consume the delay param silently

    let reason = make_dom_exception(ctx, "TimeoutError", "The operation timed out.")?;

    let inner = Rc::new(RefCell::new(SignalInner {
        aborted: true,
        reason,
        controller: None,
        notify: Arc::new(Notify::new()),
        on_abort_body: None,
        followers: Vec::new(),
        listeners: Vec::new(),
    }));
    inner.borrow().notify.notify_waiters();

    let signal =
        JSAbortSignal::new_instance(ctx, JSValue::UNDEFINED, AbortSignalPayload { inner })?;
    Ok(signal.into())
}

// ===========================================================================
// AbortController
// ===========================================================================

pub(crate) struct AbortControllerPayload {
    signal: Object,
    inner: Rc<RefCell<SignalInner>>,
}

fn get_controller<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a AbortControllerPayload, i32> {
    JSAbortController::payload_of(val)
        .ok_or_else(|| ctx.throw_internal_error("not an AbortController instance"))
}

fn js_controller_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    let inner = Rc::new(RefCell::new(SignalInner {
        aborted: false,
        reason: JSValue::UNDEFINED,
        controller: None,
        notify: Arc::new(Notify::new()),
        on_abort_body: None,
        followers: Vec::new(),
        listeners: Vec::new(),
    }));

    let signal = JSAbortSignal::new_instance(
        ctx,
        JSValue::UNDEFINED,
        AbortSignalPayload {
            inner: inner.clone(),
        },
    )?;

    let payload = AbortControllerPayload {
        signal: signal.0,
        inner: inner.clone(),
    };

    let controller = JSAbortController::new_instance(ctx, new_target, payload)?;

    // Link signal back to controller
    inner.borrow_mut().controller = Some(controller.0);

    Ok(controller.into())
}

fn js_controller_signal_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let controller = get_controller(ctx, &this)?;
    Ok(controller.signal.into())
}

fn js_controller_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let controller = get_controller(ctx, &this)?;

    let (reason, on_abort_body) = {
        let mut inner = controller.inner.borrow_mut();
        if inner.aborted {
            return Ok(JSValue::UNDEFINED);
        }
        inner.aborted = true;
        // Spec: `AbortController.abort(reason = new DOMException('...', 'AbortError'))`.
        // An explicitly passed `undefined` still triggers the default.
        let reason = if cc.length() > 0 && !cc.arg(0).is_undefined() {
            cc.arg(0)
        } else {
            make_dom_exception(ctx, "AbortError", "The operation was aborted.")?
        };
        inner.reason = reason;
        let body = inner.on_abort_body;
        inner.notify.notify_waiters();
        (reason, body)
    };

    if let Some(body) = on_abort_body {
        if let Some(cancel_fn) = body
            .get_property_str(ctx, "cancel")
            .ok()
            .filter(|v| v.is_callable())
        {
            if cancel_fn.call(ctx, body, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
        }
    }

    // Fire this signal's own `abort` listeners, then propagate to any "follow"
    // signals (e.g. `request.signal`).
    fire_abort_events(ctx, &controller.inner);
    propagate_abort(ctx, &controller.inner, reason);

    Ok(JSValue::UNDEFINED)
}

// ===========================================================================
// Registration: AbortSignal
// ===========================================================================

const CLASS_SIGNAL_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("aborted", Some(js_signal_aborted_getter), None, 0),
    FieldDef::GETSET("reason", Some(js_signal_reason_getter), None, 0),
    FieldDef::GETSET(
        "onabort",
        Some(js_signal_onabort_getter),
        Some(js_signal_onabort_setter),
        0,
    ),
    FieldDef::FUNC("throwIfAborted", 0, js_signal_throw_if_aborted, 0),
    // EventTarget subset (AbortSignal is an EventTarget per WHATWG).
    FieldDef::FUNC("addEventListener", 0, js_signal_add_event_listener, 0),
    FieldDef::FUNC("removeEventListener", 0, js_signal_remove_event_listener, 0),
    FieldDef::FUNC("dispatchEvent", 0, js_signal_dispatch_event, 0),
];

def_native_object!(
    "AbortSignal",
    JSAbortSignal,
    AbortSignalPayload,
    CLASS_SIGNAL_PROTO,
    Some(ClassCtorDef::new(js_signal_constructor, 0, true).expose_to_global()),
    on_payload_gc: AbortSignalPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_abort_signal(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSAbortSignal::register(rt)
}

// ===========================================================================
// Registration: AbortController
// ===========================================================================

const CLASS_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("signal", Some(js_controller_signal_getter), None, 0),
    FieldDef::FUNC("abort", 1, js_controller_abort, 0),
];

def_native_object!(
    "AbortController",
    JSAbortController,
    AbortControllerPayload,
    CLASS_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_controller_constructor, 0, true).expose_to_global()),
    on_payload_gc: AbortControllerPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_abort_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSAbortController::register(rt)
}

impl AbortSignalPayload {
    pub(crate) fn is_aborted(&self) -> bool {
        self.inner.borrow().aborted
    }

    pub(crate) fn notify(&self) -> Arc<Notify> {
        self.inner.borrow().notify.clone()
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        let inner = self.inner.borrow();
        inner.reason.trace(gcx);
        if let Some(ref body) = inner.on_abort_body {
            body.trace(gcx);
        }
        if let Some(ref c) = inner.controller {
            c.trace(gcx);
        }
        for (_, cb) in &inner.listeners {
            cb.trace(gcx);
        }
    }
}

impl AbortControllerPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        // Keep the associated signal alive: the controller→signal link is the
        // only GC-traceable edge into the shared signal state for a signal
        // referenced solely via `AbortController.signal`.
        self.signal.trace(gcx);
    }
}

/// Get the abort `Notify` for a JS `AbortSignal` value, if it is one.
pub(crate) fn signal_notify(val: &JSValue) -> Option<Arc<Notify>> {
    JSAbortSignal::payload_of(val).map(|p| p.notify())
}

/// Whether a JS `AbortSignal` value is already aborted.
pub(crate) fn signal_is_aborted(val: &JSValue) -> bool {
    JSAbortSignal::payload_of(val)
        .map(|p| p.is_aborted())
        .unwrap_or(false)
}

/// The abort reason of a JS `AbortSignal` value, if it is aborted.
///
/// Per the Fetch spec, a fetch cancelled by a signal must reject with the
/// signal's `reason` (e.g. a DOMException "AbortError" or whatever was passed
/// to `abort()`), not a freshly-constructed error.
pub(crate) fn signal_reason(val: &JSValue) -> Option<JSValue> {
    let p = JSAbortSignal::payload_of(val)?;
    if p.is_aborted() {
        Some(p.inner.borrow().reason)
    } else {
        None
    }
}

/// Make `child` a "follower" of `parent`: when `parent` aborts, `child` aborts
/// with the same reason. If `parent` is already aborted, `child` is aborted
/// immediately. Used by `Request` to implement WHATWG "follow" signal semantics
/// (`request.signal` is never the same object as the signal passed to it).
pub(crate) fn signal_attach_follower(ctx: &mut ScopedContext, parent: &JSValue, child: &JSValue) {
    let Some(parent_p) = JSAbortSignal::payload_of(parent) else {
        return;
    };
    let Some(child_p) = JSAbortSignal::payload_of(child) else {
        return;
    };
    let parent_inner = parent_p.inner.clone();
    let child_inner = child_p.inner.clone();
    let mut p = parent_inner.borrow_mut();
    if p.aborted {
        // Already aborted: mirror the state onto the child right now.
        let mut c = child_inner.borrow_mut();
        c.aborted = true;
        c.reason = p.reason;
        c.notify.notify_waiters();
        drop(c);
        fire_abort_events(ctx, &child_inner);
    } else {
        p.followers.push(child_inner);
    }
}

/// Propagate an abort (with `reason`) to `self` and all of its followers.
/// Called by `AbortController.abort()` / `AbortSignal.abort()` once the
/// controller's own signal is aborted.
fn propagate_abort(ctx: &mut ScopedContext, inner: &Rc<RefCell<SignalInner>>, reason: JSValue) {
    // Collect followers first so we don't recurse while borrowing `inner`.
    let followers: Vec<Rc<RefCell<SignalInner>>> = inner.borrow().followers.clone();
    for f in &followers {
        {
            let mut fi = f.borrow_mut();
            fi.aborted = true;
            fi.reason = reason;
            fi.notify.notify_waiters();
        }
        fire_abort_events(ctx, f);
        propagate_abort(ctx, f, reason);
    }
}

/// Mark a (non-aborted) `AbortSignal` value as aborted with `reason`:
/// set `aborted`/`reason`, notify waiters, and fire `abort` event listeners.
/// Used by `WritableStream`/`WritableStreamDefaultWriter` `abort()` to
/// synchronously signal the controller's `signal` per the Streams spec.
///
/// Per the DOM spec's `AbortController.abort()`, when no reason is given
/// the default is `new DOMException("The operation was aborted.",
/// "AbortError")`.
pub(crate) fn abort_signal(
    ctx: &mut ScopedContext,
    signal_val: JSValue,
    reason: JSValue,
) -> Result<(), i32> {
    if let Some(payload) = JSAbortSignal::payload_of(&signal_val) {
        // Per DOM AbortController.abort(): if reason is not given, create
        // a default DOMException.
        let reason = if reason.is_undefined() || reason.is_null() {
            make_dom_exception(ctx, "AbortError", "The operation was aborted.")?
        } else {
            reason
        };
        let inner = payload.inner.clone();
        {
            let mut gi = inner.borrow_mut();
            gi.aborted = true;
            gi.reason = reason;
            gi.notify.notify_waiters();
        }
        fire_abort_events(ctx, &inner);
    }
    Ok(())
}

/// Create a fresh, non-aborted `AbortSignal` value. Used as the default
/// `request.signal` when none is supplied to `Request`.
pub(crate) fn make_default_signal(ctx: &mut ScopedContext) -> JSResult {
    let inner = Rc::new(RefCell::new(SignalInner {
        aborted: false,
        reason: JSValue::UNDEFINED,
        controller: None,
        notify: Arc::new(Notify::new()),
        on_abort_body: None,
        followers: Vec::new(),
        listeners: Vec::new(),
    }));
    let signal =
        JSAbortSignal::new_instance(ctx, JSValue::UNDEFINED, AbortSignalPayload { inner })?;
    Ok(signal.into())
}

pub(crate) fn install_abort_signal_statics(
    ctx: &mut ScopedContext,
    global: &mut Object,
) -> Result<(), i32> {
    // Register AbortSignal.abort() static method
    let abort_fn =
        ctx.create_native_function(zuqe::Callback::Func(js_signal_static_abort), "abort", 1, 0)?;

    // Get the AbortSignal constructor from global
    let ctor_val = global.get_property_str(ctx, "AbortSignal")?;
    if let Some(mut ctor) = ctor_val.as_object() {
        ctor.define_property_value_str(ctx, "abort", abort_fn.into(), zuqe::JS_PROP_C_W_E)?;
    }

    // Register AbortSignal.timeout() static method
    let timeout_fn = ctx.create_native_function(
        zuqe::Callback::Func(js_signal_static_timeout),
        "timeout",
        1,
        0,
    )?;

    let ctor_val = global.get_property_str(ctx, "AbortSignal")?;
    if let Some(mut ctor) = ctor_val.as_object() {
        ctor.define_property_value_str(ctx, "timeout", timeout_fn.into(), zuqe::JS_PROP_C_W_E)?;
    }

    Ok(())
}
