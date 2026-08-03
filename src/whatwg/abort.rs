// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG DOM Standard — AbortController and AbortSignal implementations.
//!
//! Provides `AbortController` and `AbortSignal` for cooperative cancellation
//! of asynchronous operations, following the WHATWG spec.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use tokio::sync::Notify;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, Exception, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object, gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::dom::make_dom_exception,
};

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

/// Static method: AbortSignal.abort(reason?)
fn js_signal_static_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = if cc.length() > 0 {
        cc.arg(0)
    } else {
        // Spec default: a DOMException named "AbortError".
        make_dom_exception(ctx, "AbortError", "The operation was aborted.")?
    };

    let inner = Rc::new(RefCell::new(SignalInner {
        aborted: true,
        reason,
        controller: None,
        notify: Arc::new(Notify::new()),
        on_abort_body: None,
        followers: Vec::new(),
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
    }));

    let signal = JSAbortSignal::new_instance(
        ctx,
        JSValue::UNDEFINED,
        AbortSignalPayload {
            inner: inner.clone(),
        },
    )?;

    inner.borrow_mut().controller = None;

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

    let mut inner = controller.inner.borrow_mut();
    if inner.aborted {
        // Already aborted — no-op per spec
        return Ok(JSValue::UNDEFINED);
    }
    inner.aborted = true;
    let reason = if cc.length() > 0 {
        cc.arg(0)
    } else {
        // Spec default: a DOMException named "AbortError".
        make_dom_exception(ctx, "AbortError", "The operation was aborted.")?
    };
    inner.reason = reason;
    // Per Fetch spec, aborting a request with a streaming body must cancel that
    // body with the abort reason (so e.g. a ReadableStream body's cancel()
    // callback runs). The body is a GC-traced JSValue stored on the signal.
    let on_abort_body = inner.on_abort_body;
    inner.notify.notify_waiters();
    // Drop the mutable borrow before propagation (propagate_abort re-borrows).
    drop(inner);

    if let Some(body) = on_abort_body {
        if let Some(cancel_fn) = body.get_property_str(ctx, "cancel").ok().filter(|v| v.is_callable())
        {
            let _ = cancel_fn.call(ctx, body, &[reason]);
        }
    }
    // Propagate to any "follow" signals (e.g. `request.signal`).
    propagate_abort(&controller.inner, reason);

    Ok(JSValue::UNDEFINED)
}

// ===========================================================================
// Registration: AbortSignal
// ===========================================================================

const CLASS_SIGNAL_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("aborted", Some(js_signal_aborted_getter), None, 0),
    FieldDef::GETSET("reason", Some(js_signal_reason_getter), None, 0),
    FieldDef::FUNC("throwIfAborted", 0, js_signal_throw_if_aborted, 0),
];

def_native_object!(
    "AbortSignal",
    JSAbortSignal,
    AbortSignalPayload,
    CLASS_SIGNAL_PROTO,
    Some(ClassCtorDef::new(js_signal_constructor, 0, true).expose_to_global()),
    on_payload_gc: AbortSignalPayload::gc_trace
);

// Static methods are registered after class creation,
// handled via the `static_fields` concept or by manual registration.
// We register them in the expose phase below.

fn register_abort_signal_statics(ctx: &mut ScopedContext, global: &mut Object) -> Result<(), i32> {
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
    Some(ClassCtorDef::new(js_controller_constructor, 0, true).expose_to_global())
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
    }
}

/// Get the abort `Notify` for a JS `AbortSignal` value, if it is one.
pub(crate) fn signal_notify(val: &JSValue) -> Option<Arc<Notify>> {
    JSAbortSignal::payload_of(val).map(|p| p.notify())
}

/// Attach a request body (e.g. a `ReadableStream`) to a JS `AbortSignal` so that,
/// when the signal aborts, the body is canceled with the abort reason (Fetch spec).
pub(crate) fn signal_set_body(val: &JSValue, body: JSValue) {
    if let Some(p) = JSAbortSignal::payload_of(val) {
        p.inner.borrow_mut().on_abort_body = Some(body);
    }
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
pub(crate) fn signal_attach_follower(parent: &JSValue, child: &JSValue) {
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
    } else {
        p.followers.push(child_inner);
    }
}

/// Propagate an abort (with `reason`) to `self` and all of its followers.
/// Called by `AbortController.abort()` / `AbortSignal.abort()` once the
/// controller's own signal is aborted.
fn propagate_abort(inner: &Rc<RefCell<SignalInner>>, reason: JSValue) {
    // Collect followers first so we don't recurse while borrowing `inner`.
    let followers: Vec<Rc<RefCell<SignalInner>>> = inner.borrow().followers.clone();
    for f in &followers {
        {
            let mut fi = f.borrow_mut();
            fi.aborted = true;
            fi.reason = reason;
            fi.notify.notify_waiters();
        }
        propagate_abort(f, reason);
    }
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
    }));
    let signal =
        JSAbortSignal::new_instance(ctx, JSValue::UNDEFINED, AbortSignalPayload { inner })?;
    Ok(signal.into())
}

/// Called during `install_whatwg_globals` to wire up static methods
/// on AbortSignal (since `def_native_object!` doesn't handle statics).
pub(crate) fn install_abort_statics(
    ctx: &mut ScopedContext,
    global: &mut Object,
) -> Result<(), i32> {
    register_abort_signal_statics(ctx, global)
}
