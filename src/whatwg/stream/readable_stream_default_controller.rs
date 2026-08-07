// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableStreamDefaultController: the controller handed to an underlying
//! source's `start()`/`pull()` callbacks.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, JobState, Runtime,
    ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, whatwg::stream::readable_stream::JSReadableStream};

use super::{QueueEntry, RsState, make_iterator_result};

// ============================================================================
// ReadableStreamDefaultController
// ============================================================================

pub(crate) struct ReadableStreamDefaultControllerPayload {
    pub(crate) stream: Option<JSReadableStream>,
    pub(crate) underlying_source: JSValue,
    pub(crate) queue: Rc<RefCell<Vec<QueueEntry>>>,
    pub(crate) queue_total_size: Rc<Cell<f64>>,
    pub(crate) high_water_mark: f64,
    pub(crate) started: Cell<bool>,
    pub(crate) close_requested: Cell<bool>,
    pub(crate) pull_again: Cell<bool>,
    pub(crate) pulling: Cell<bool>,
    /// Queue of in-flight read() resolvers awaiting data (each is [resolve,
    /// reject]). Kept as a queue so concurrent read() calls each get their own
    /// resolver instead of clobbering the previous one. Stored per-controller
    /// (not globally) so concurrent/sequential streams do not steal chunks.
    pub(crate) pending_read: Rc<RefCell<Vec<[JSValue; 2]>>>,
    /// Self JS handle, set right after instance creation, used to re-queue
    /// pull jobs.
    pub(crate) controller: Option<JSReadableStreamDefaultController>,
}

impl ReadableStreamDefaultControllerPayload {
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
            started: Cell::new(false),
            close_requested: Cell::new(false),
            pull_again: Cell::new(false),
            pulling: Cell::new(false),
            pending_read: Rc::new(RefCell::new(Vec::new())),
            controller: None,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_source.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        for entry in self.queue.borrow().iter() {
            entry.chunk.trace(gcx);
        }
        for rr in self.pending_read.borrow().iter() {
            rr[0].trace(gcx);
            rr[1].trace(gcx);
        }
    }

    /// Resolve the controller's pending read (if any) with an iterator result
    /// `{value, done}`. Returns true if a pending read was present and resolved.
    /// Resolve the controller's *oldest* pending read (if any) with an iterator
    /// result `{value, done}`. Returns true if a pending read was resolved.
    pub(super) fn resolve_pending_read(
        &self,
        ctx: &mut ScopedContext,
        value: JSValue,
        done: bool,
    ) -> bool {
        let rr = self.pending_read.borrow_mut().first().copied();
        if let Some([resolve, _reject]) = rr {
            if let Ok(result) = make_iterator_result(ctx, value, done) {
                self.pending_read.borrow_mut().remove(0);
                if resolve.call(ctx, JSValue::UNDEFINED, &[result]).is_err() {
                    let _e = ctx.take_exception();
                    #[cfg(debug_assertions)]
                    eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
                }
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Reject the controller's *oldest* pending read with `reason`.
    pub(super) fn reject_pending_read(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let rr = self.pending_read.borrow_mut().first().copied();
        if let Some([_resolve, reject]) = rr {
            self.pending_read.borrow_mut().remove(0);
            // fn returns (); a throwing resolver is abnormal, drop its exception.
            if reject.call(ctx, JSValue::UNDEFINED, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
        }
    }

    /// Reject every outstanding pending read with `reason` (used on error).
    pub(super) fn reject_all_pending_reads(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let reads = self.pending_read.borrow_mut().drain(..).collect::<Vec<_>>();
        for [_, reject] in reads {
            if reject.call(ctx, JSValue::UNDEFINED, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
        }
    }

    /// Ask the controller to pull one more chunk (call its underlying source's
    /// pull, or refill the queue if strategy is undefined).
    pub(super) fn call_controller_pull(&self, ctx: &mut ScopedContext) -> Result<(), i32> {
        if self.pulling.get() {
            self.pull_again.set(true);
            return Ok(());
        }

        self.pulling.set(true);
        self.started.set(true);

        let source = &self.underlying_source;
        if source.is_undefined() {
            self.pulling.set(false);
            return Ok(());
        }

        let obj = match source.ensure_object(ctx) {
            Ok(o) => o,
            Err(_) => {
                self.pulling.set(false);
                return Ok(());
            }
        };

        let pull_fn = match obj.get_property_str(ctx, "pull") {
            Ok(v) if v.is_callable() => v,
            _ => {
                self.pulling.set(false);
                return Ok(());
            }
        };

        let controller_val = self
            .controller
            .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

        // Spec: if the underlying source's pull() throws, error the stream with
        // that value (which rejects any pending reads) rather than swallowing it.
        let pull_res = pull_fn.call(ctx, *source, &[JSValue::from(controller_val)]);
        if pull_res.is_err() {
            let err = ctx.take_exception().to_value(ctx);
            self.pulling.set(false);
            self.reject_all_pending_reads(ctx, err);
            self.queue.borrow_mut().clear();
            self.queue_total_size.set(0.0);
            if let Some(mut stream_val) = self.stream {
                stream_val
                    .payload_mut()
                    .unwrap()
                    .settle_state(ctx, RsState::Errored, Some(err));
            }
            return Ok(());
        }

        self.pulling.set(false);

        if self.pull_again.get() {
            self.pull_again.set(false);
            let ctrl = controller_val;
            ctx.queue_job(
                move |cx| {
                    let p = ctrl
                        .payload()
                        .ok_or_else(|| cx.throw_internal_error("invalid controller"))?;
                    p.call_controller_pull(cx)?;
                    Ok(JobState::Done)
                },
                (),
            );
        }

        Ok(())
    }
}

const CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("enqueue", 1, js_controller_enqueue, 0),
    FieldDef::FUNC("close", 0, js_controller_close, 0),
    FieldDef::FUNC("error", 1, js_controller_error, 0),
    FieldDef::GETSET("desiredSize", Some(js_controller_desired_size), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "ReadableStreamDefaultController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("ReadableStreamDefaultController cannot be constructed directly"))
}

def_native_object!(
    "ReadableStreamDefaultController",
    JSReadableStreamDefaultController,
    ReadableStreamDefaultControllerPayload,
    CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: ReadableStreamDefaultControllerPayload::gc_trace
);

fn js_controller_enqueue(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chunk = cc.arg(0);
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    // Spec: enqueue must throw when the stream is already closed or errored.
    if let Some(stream_val) = p.stream {
        if let Some(sp) = stream_val.payload() {
            if sp.state == RsState::Errored {
                return Err(ctx.throw_type_error("cannot enqueue to an errored stream"));
            }
            if sp.state == RsState::Closed {
                return Err(ctx.throw_type_error("cannot enqueue to a closed stream"));
            }
        }
    }

    if p.close_requested.get() {
        return Err(ctx.throw_type_error("cannot enqueue after close"));
    }

    let resolved = p.resolve_pending_read(ctx, chunk, false);

    if !resolved {
        p.queue.borrow_mut().push(QueueEntry { chunk, size: 1.0 });
        let total = p.queue_total_size.get();
        p.queue_total_size.set(total + 1.0);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    // Spec: close must throw when the stream is already closed or errored.
    if let Some(stream_val) = p.stream
        && let Some(sp) = stream_val.payload()
    {
        if sp.state == RsState::Errored {
            return Err(ctx.throw_type_error("cannot close an errored stream"));
        }
        if sp.state == RsState::Closed {
            return Err(ctx.throw_type_error("cannot close a closed stream"));
        }
    }

    p.close_requested.set(true);

    // Every outstanding read() past the queue now resolves as done.
    while p.resolve_pending_read(ctx, JSValue::UNDEFINED, true) {}

    // Per spec: only transition to closed when queue is empty.
    // If queue still has items, state stays "readable" until last
    // item is consumed by read().
    if p.queue.borrow().is_empty()
        && let Some(mut rs) = p.stream
    {
        rs.payload_mut()
            .unwrap()
            .settle_state(ctx, RsState::Closed, None);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let e = cc.arg(0);
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;

    // Spec: error must throw when the stream is already closed or errored.
    if let Some(stream_val) = p.stream {
        if let Some(sp) = stream_val.payload() {
            if sp.state == RsState::Errored {
                return Err(ctx.throw_type_error("cannot error an errored stream"));
            }
            if sp.state == RsState::Closed {
                return Err(ctx.throw_type_error("cannot error a closed stream"));
            }
        }
    }

    // Reject every outstanding read() with the error, and drop any queued
    // chunks (spec: an errored stream discards its queue, so later reads
    // reject instead of draining stale data).
    p.reject_all_pending_reads(ctx, e);
    p.queue.borrow_mut().clear();
    p.queue_total_size.set(0.0);

    if let Some(mut rs) = p.stream {
        rs.payload_mut()
            .unwrap()
            .settle_state(ctx, RsState::Errored, Some(e));
    }

    Ok(JSValue::UNDEFINED)
}

fn js_controller_desired_size(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSReadableStreamDefaultController::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStreamDefaultController"))?;
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

#[distributed_slice(USER_CLASS_DEFS)]
fn register_readable_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSReadableStreamDefaultController::register(rt)
}
