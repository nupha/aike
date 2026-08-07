// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Unified readable stream controller payload.
//!
//! `ReadableStreamDefaultController` and `ReadableByteStreamController` share a
//! single payload enum `ReadableControllerPayload`. Operations are exposed as
//! methods on the enum and dispatch on the variant internally, so callers use
//! one uniform API without caring which controller kind they hold.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use zuqe::{
    JSResult, JSValue, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

use super::{
    JSReadableByteStreamController, JSReadableStream, JSReadableStreamDefaultController,
    QueueEntry, RsState, make_iterator_result,
};

/// Fields that differ between the default and byte controller kinds. Common
/// fields (stream, underlying_source, queue, queue_total_size, high_water_mark,
/// close_requested) live on `ReadableControllerPayload` directly.
pub(crate) enum ReadableControllerKind {
    Default {
        /// Self JS handle, set right after instance creation, used to re-queue
        /// pull jobs.
        controller: Option<JSReadableStreamDefaultController>,
        started: Cell<bool>,
        pulling: Cell<bool>,
        pull_again: Cell<bool>,
        /// Queue of in-flight read() resolvers awaiting data (each is [resolve,
        /// reject]). Kept as a queue so concurrent read() calls each get their own
        /// resolver instead of clobbering the previous one. Stored per-controller
        /// (not globally) so concurrent/sequential streams do not steal chunks.
        pending_read: Rc<RefCell<Vec<[JSValue; 2]>>>,
    },

    /// Minimal byte controller (constructor + toStringTag only in this build).
    Byte {
        /// Pending BYOB request view, if any. Left `None` in this minimal build.
        byob_request: Rc<RefCell<Option<JSValue>>>,
    },
}

/// Unified controller payload
pub(crate) struct ReadableControllerPayload {
    pub(crate) stream: Option<JSReadableStream>,
    pub(crate) underlying_source: JSValue,
    pub(crate) queue: Rc<RefCell<Vec<QueueEntry>>>,
    pub(crate) queue_total_size: Rc<Cell<i32>>,
    pub(crate) high_water_mark: i32,
    pub(crate) close_requested: Cell<bool>,
    pub(crate) kind: ReadableControllerKind,
}

impl ReadableControllerPayload {
    pub(crate) fn new_default(
        underlying_source: JSValue,
        stream: Option<JSReadableStream>,
        high_water_mark: i32,
    ) -> Self {
        Self {
            stream,
            underlying_source,
            queue: Rc::new(RefCell::new(Vec::new())),
            queue_total_size: Rc::new(Cell::new(0)),
            high_water_mark,
            close_requested: Cell::new(false),
            kind: ReadableControllerKind::Default {
                controller: None,
                started: Cell::new(false),
                pulling: Cell::new(false),
                pull_again: Cell::new(false),
                pending_read: Rc::new(RefCell::new(Vec::new())),
            },
        }
    }

    pub(crate) fn new_byte(
        underlying_source: JSValue,
        stream: Option<JSReadableStream>,
        high_water_mark: i32,
    ) -> Self {
        Self {
            stream,
            underlying_source,
            queue: Rc::new(RefCell::new(Vec::new())),
            queue_total_size: Rc::new(Cell::new(0)),
            high_water_mark,
            close_requested: Cell::new(false),
            kind: ReadableControllerKind::Byte {
                byob_request: Rc::new(RefCell::new(None)),
            },
        }
    }

    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_source.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        for entry in self.queue.borrow().iter() {
            entry.chunk.trace(gcx);
        }
        match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => {
                for rr in pending_read.borrow().iter() {
                    rr[0].trace(gcx);
                    rr[1].trace(gcx);
                }
            }
            ReadableControllerKind::Byte { byob_request } => {
                if let Some(ref br) = *byob_request.borrow() {
                    br.trace(gcx);
                }
            }
        }
    }

    /// The controlled readable stream, if attached.
    pub(crate) fn stream(&self) -> Option<JSReadableStream> {
        self.stream
    }

    /// The underlying source object handed to `start()`/`pull()`.
    pub(crate) fn underlying_source(&self) -> JSValue {
        self.underlying_source
    }

    /// Pending read resolvers (read() callbacks awaiting a chunk). Byte streams
    /// have no pending read concept, so this is unreachable there.
    pub(crate) fn pending_read(&self) -> &Rc<RefCell<Vec<[JSValue; 2]>>> {
        match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => pending_read,
            ReadableControllerKind::Byte { .. } => {
                unreachable!("byte streams have no pending read queue")
            }
        }
    }

    /// Wire the controller's self-handle (call_controller_pull needs it).
    pub(crate) fn set_controller(&mut self, c: Option<JSReadableStreamDefaultController>) {
        if let ReadableControllerKind::Default { controller, .. } = &mut self.kind {
            *controller = c;
        } else {
            unreachable!("not for byte streams controller")
        }
    }

    pub(crate) fn high_water_mark(&self) -> i32 {
        self.high_water_mark
    }

    pub(crate) fn close_requested(&self) -> bool {
        self.close_requested.get()
    }

    pub(crate) fn set_close_requested(&self, v: bool) {
        self.close_requested.set(v);
    }

    /// Queue of buffered chunks.
    pub(crate) fn queue(&self) -> &Rc<RefCell<Vec<QueueEntry>>> {
        &self.queue
    }

    pub(crate) fn queue_total_size(&self) -> i32 {
        self.queue_total_size.get()
    }

    pub(crate) fn set_queue_total_size(&self, v: i32) {
        self.queue_total_size.set(v);
    }

    /// Resolve the controller's *oldest* pending read (if any) with an iterator
    /// result `{value, done}`. Returns true if a pending read was resolved.
    pub(crate) fn resolve_pending_read(
        &self,
        ctx: &mut ScopedContext,
        value: JSValue,
        done: bool,
    ) -> bool {
        let pending_read = match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => pending_read,
            ReadableControllerKind::Byte { .. } => return false,
        };
        let rr = pending_read.borrow_mut().first().copied();
        if let Some([resolve, _reject]) = rr {
            if let Ok(result) = make_iterator_result(ctx, value, done) {
                pending_read.borrow_mut().remove(0);
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
    pub(crate) fn reject_pending_read(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let pending_read = match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => pending_read,
            ReadableControllerKind::Byte { .. } => return,
        };
        let rr = pending_read.borrow_mut().first().copied();
        if let Some([_resolve, reject]) = rr {
            pending_read.borrow_mut().remove(0);
            // fn returns (); a throwing resolver is abnormal, drop its exception.
            if reject.call(ctx, JSValue::UNDEFINED, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
        }
    }

    /// Reject every outstanding pending read with `reason` (used on error).
    pub(crate) fn reject_all_pending_reads(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let pending_read = match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => pending_read,
            ReadableControllerKind::Byte { .. } => return,
        };
        let reads = pending_read.borrow_mut().drain(..).collect::<Vec<_>>();
        for [_, reject] in reads {
            if reject.call(ctx, JSValue::UNDEFINED, &[reason]).is_err() {
                let _e = ctx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] {}:{}: {:?}", file!(), line!(), _e);
            }
        }
    }

    /// Ask the underlying source to pull one more chunk. Byte streams do not
    /// use pull-based filling yet, so this is a no-op there.
    pub(crate) fn call_pull(&self, ctx: &mut ScopedContext) -> Result<(), i32> {
        let (pulling, started, pull_again, controller, pending_read) = match &self.kind {
            ReadableControllerKind::Default {
                pulling,
                started,
                pull_again,
                controller,
                pending_read,
            } => (pulling, started, pull_again, controller, pending_read),
            ReadableControllerKind::Byte { .. } => return Ok(()),
        };

        if pulling.get() {
            pull_again.set(true);
            return Ok(());
        }

        pulling.set(true);
        started.set(true);

        let source = &self.underlying_source;
        if source.is_undefined() {
            pulling.set(false);
            return Ok(());
        }

        let obj = match source.ensure_object(ctx) {
            Ok(o) => o,
            Err(_) => {
                pulling.set(false);
                return Ok(());
            }
        };

        let pull_fn = match obj.get_property_str(ctx, "pull") {
            Ok(v) if v.is_callable() => v,
            _ => {
                pulling.set(false);
                return Ok(());
            }
        };

        let controller_val =
            controller.ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

        // Spec: if the underlying source's pull() throws, error the stream with
        // that value (which rejects any pending reads) rather than swallowing it.
        let pull_res = pull_fn.call(ctx, *source, &[JSValue::from(controller_val)]);
        if pull_res.is_err() {
            let err = ctx.take_exception().to_value(ctx);
            pulling.set(false);
            self.reject_all_pending_reads(ctx, err);
            self.queue.borrow_mut().clear();
            self.queue_total_size.set(0);
            if let Some(mut stream_val) = self.stream {
                stream_val
                    .payload_mut()
                    .unwrap()
                    .settle_state(ctx, RsState::Errored, Some(err));
            }
            return Ok(());
        }

        pulling.set(false);

        if pull_again.get() {
            pull_again.set(false);
            ctx.queue_job(
                move |cx| {
                    let p = controller_val
                        .payload()
                        .ok_or_else(|| cx.throw_internal_error("invalid controller"))?;
                    p.call_pull(cx)?;
                    Ok(zuqe::JobState::Done)
                },
                (),
            );
        }

        Ok(())
    }

    /// Enqueue a chunk, throwing if the stream is already closed/errored.
    pub(crate) fn enqueue(&self, ctx: &mut ScopedContext, chunk: JSValue) -> JSResult {
        if let Some(rs) = self.stream
            && let Some(sp) = rs.payload()
        {
            if sp.state == RsState::Errored {
                return Err(ctx.throw_type_error("cannot enqueue to an errored stream"));
            }
            if sp.state == RsState::Closed {
                return Err(ctx.throw_type_error("cannot enqueue to a closed stream"));
            }
        }

        if self.close_requested.get() {
            return Err(ctx.throw_type_error("cannot enqueue after close"));
        }

        match &self.kind {
            ReadableControllerKind::Default { .. } => {
                let resolved = self.resolve_pending_read(ctx, chunk, false);
                if !resolved {
                    self.queue.borrow_mut().push(QueueEntry { chunk, size: 1 });
                    self.queue_total_size.set(self.queue_total_size.get() + 1);
                }
                Ok(JSValue::UNDEFINED)
            }
            ReadableControllerKind::Byte { .. } => {
                self.queue.borrow_mut().push(QueueEntry { chunk, size: 1 });
                self.queue_total_size.set(self.queue_total_size.get() + 1);
                Ok(JSValue::UNDEFINED)
            }
        }
    }

    /// Close the controlled stream.
    pub(crate) fn close(&self, ctx: &mut ScopedContext) -> Result<(), i32> {
        if let Some(rs) = self.stream {
            if let Some(sp) = rs.payload() {
                if sp.state == RsState::Errored {
                    return Err(ctx.throw_type_error("cannot close an errored stream"));
                }
                if sp.state == RsState::Closed {
                    return Err(ctx.throw_type_error("cannot close a closed stream"));
                }
            }
        }

        self.close_requested.set(true);

        match &self.kind {
            ReadableControllerKind::Default { .. } => {
                while self.resolve_pending_read(ctx, JSValue::UNDEFINED, true) {}
                if self.queue.borrow().is_empty()
                    && let Some(mut rs) = self.stream
                {
                    rs.payload_mut()
                        .unwrap()
                        .settle_state(ctx, RsState::Closed, None);
                }
            }
            ReadableControllerKind::Byte { .. } => {
                if self.queue.borrow().is_empty()
                    && let Some(mut rs) = self.stream
                {
                    rs.payload_mut()
                        .unwrap()
                        .settle_state(ctx, RsState::Closed, None);
                }
            }
        }

        Ok(())
    }

    /// Error the controlled stream with `e`.
    pub(crate) fn error(&self, ctx: &mut ScopedContext, e: JSValue) -> Result<(), i32> {
        if let Some(rs) = self.stream
            && let Some(sp) = rs.payload()
        {
            if sp.state == RsState::Errored {
                return Err(ctx.throw_type_error("cannot error an errored stream"));
            }
            if sp.state == RsState::Closed {
                return Err(ctx.throw_type_error("cannot error a closed stream"));
            }
        }

        match &self.kind {
            ReadableControllerKind::Default { .. } => {
                self.reject_all_pending_reads(ctx, e);
                self.queue.borrow_mut().clear();
                self.queue_total_size.set(0);
                if let Some(mut rs) = self.stream {
                    rs.payload_mut()
                        .unwrap()
                        .settle_state(ctx, RsState::Errored, Some(e));
                }
            }
            ReadableControllerKind::Byte { .. } => {
                if let Some(mut rs) = self.stream {
                    rs.payload_mut()
                        .unwrap()
                        .settle_state(ctx, RsState::Errored, Some(e));
                }
            }
        }

        Ok(())
    }

    /// Spec: errored -> null (signalled as `i32::MAX`), closed -> 0, else
    /// highWaterMark - queueTotalSize. The result can be negative when the
    /// queue is over-full; that is the spec-mandated backpressure signal.
    pub(crate) fn desired_size(&self, _: &mut ScopedContext) -> Result<i32, i32> {
        if let Some(s) = self.stream
            && let Some(sp) = s.payload()
        {
            match sp.state {
                RsState::Errored => return Ok(i32::MAX),
                RsState::Closed => return Ok(0),
                RsState::Readable => {}
            }
        }

        Ok(self.high_water_mark - self.queue_total_size.get())
    }

    /// Pending BYOB request view, or undefined. Only meaningful for byte streams.
    pub(crate) fn byob_request(&self) -> JSValue {
        match &self.kind {
            ReadableControllerKind::Default { .. } => JSValue::UNDEFINED,
            ReadableControllerKind::Byte { byob_request } => {
                byob_request.borrow().unwrap_or(JSValue::UNDEFINED)
            }
        }
    }
}

/// Fetch the shared `ReadableControllerPayload` from a controller `this` value,
/// accepting either the default or byte controller native type. The returned
/// borrow is tied to `this`'s lifetime.
pub(super) fn get_readable_controller<'a>(
    ctx: &mut ScopedContext,
    this: &'a JSValue,
) -> Result<&'a ReadableControllerPayload, i32> {
    if let Some(p) = JSReadableStreamDefaultController::payload_of(this) {
        return Ok(p);
    }
    if let Some(p) = JSReadableByteStreamController::payload_of(this) {
        return Ok(p);
    }
    Err(ctx.throw_type_error("not a ReadableStream controller"))
}
