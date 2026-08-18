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
    CallContext, Callback, ErrorKind, Exception, JSResult, JSValue, ScopedContext,
    builtins::promise::Promise,
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
        controller: Option<JSReadableStreamDefaultController>,
        started: Cell<bool>,
        pulling: Cell<bool>,
        pull_again: Cell<bool>,
        pending_read: Rc<RefCell<Vec<Promise>>>,
        in_tee: Cell<bool>,
        /// True when the stream was constructed with `type: 'owning'`.
        owning: Cell<bool>,
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
    pub(crate) queue_total_size: Rc<Cell<f64>>,
    pub(crate) high_water_mark: f64,
    pub(crate) close_requested: Cell<bool>,
    /// Captured at stream setup (spec `SetUpReadableStreamDefaultController`):
    /// the underlying source's `pull`/`cancel` and the strategy's `size`
    /// callbacks. `undefined` means "absent". Capturing once avoids re-running
    /// the property getters on every pull/cancel/enqueue.
    pub(crate) pull_algorithm: JSValue,
    pub(crate) cancel_algorithm: JSValue,
    pub(crate) size_algorithm: JSValue,
    pub(crate) kind: ReadableControllerKind,
}

impl ReadableControllerPayload {
    pub(crate) fn new_default(
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
            close_requested: Cell::new(false),
            pull_algorithm: JSValue::UNDEFINED,
            cancel_algorithm: JSValue::UNDEFINED,
            size_algorithm: JSValue::UNDEFINED,
            kind: ReadableControllerKind::Default {
                controller: None,
                started: Cell::new(false),
                pulling: Cell::new(false),
                pull_again: Cell::new(false),
                pending_read: Rc::new(RefCell::new(Vec::new())),
                in_tee: Cell::new(false),
                owning: Cell::new(false),
            },
        }
    }

    pub(crate) fn new_byte(
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
            close_requested: Cell::new(false),
            pull_algorithm: JSValue::UNDEFINED,
            cancel_algorithm: JSValue::UNDEFINED,
            size_algorithm: JSValue::UNDEFINED,
            kind: ReadableControllerKind::Byte {
                byob_request: Rc::new(RefCell::new(None)),
            },
        }
    }

    pub(crate) fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_source.trace(gcx);
        self.pull_algorithm.trace(gcx);
        self.cancel_algorithm.trace(gcx);
        self.size_algorithm.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        for entry in self.queue.borrow().iter() {
            entry.chunk.trace(gcx);
        }
        match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => {
                for p in pending_read.borrow().iter() {
                    p.trace(gcx);
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

    /// Underlying source algorithms captured during setup
    /// (`SetUpReadableStreamDefaultControllerFromUnderlyingSource`): `pull`,
    /// `cancel`, and the strategy's `size`. Undefined means absent.
    pub(crate) fn set_algorithms(&mut self, pull: JSValue, cancel: JSValue, size: JSValue) {
        self.pull_algorithm = pull;
        self.cancel_algorithm = cancel;
        self.size_algorithm = size;
    }

    pub(crate) fn cancel_algorithm(&self) -> JSValue {
        self.cancel_algorithm
    }

    /// The underlying source object handed to `start()`/`pull()`.
    pub(crate) fn underlying_source(&self) -> JSValue {
        self.underlying_source
    }

    /// Pending read promises (read() calls awaiting a chunk). Byte streams
    /// have no pending read concept, so this is unreachable there.
    pub(crate) fn pending_read(&self) -> &Rc<RefCell<Vec<Promise>>> {
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

    pub(crate) fn set_in_tee(&self, v: bool) {
        if let ReadableControllerKind::Default { in_tee, .. } = &self.kind {
            in_tee.set(v);
        }
    }

    pub(crate) fn set_owning(&self, v: bool) {
        if let ReadableControllerKind::Default { owning, .. } = &self.kind {
            owning.set(v);
        }
    }

    pub(crate) fn high_water_mark(&self) -> f64 {
        self.high_water_mark
    }

    pub(crate) fn close_requested(&self) -> bool {
        self.close_requested.get()
    }

    pub(crate) fn set_close_requested(&self, v: bool) {
        self.close_requested.set(v);
    }

    /// Whether `start()` has completed (the start promise fulfilled). Pull is
    /// only allowed once started (spec `[[started]]`).
    pub(crate) fn set_started(&self, v: bool) {
        if let ReadableControllerKind::Default { started, .. } = &self.kind {
            started.set(v);
        }
    }

    /// Queue of buffered chunks.
    pub(crate) fn queue(&self) -> &Rc<RefCell<Vec<QueueEntry>>> {
        &self.queue
    }

    pub(crate) fn queue_total_size(&self) -> f64 {
        self.queue_total_size.get()
    }

    pub(crate) fn set_queue_total_size(&self, v: f64) {
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
        let promise = pending_read.borrow_mut().first().copied();
        if let Some(mut p) = promise {
            if let Ok(result) = make_iterator_result(ctx, value, done) {
                pending_read.borrow_mut().remove(0);
                // Settle directly so an injected Object.prototype.then cannot
                // intercept the read result.
                p.resolve(ctx, result);
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
        let promise = pending_read.borrow_mut().first().copied();
        if let Some(mut p) = promise {
            pending_read.borrow_mut().remove(0);
            p.reject(ctx, reason);
        }
    }

    /// Reject every outstanding pending read with `reason` (used on error).
    pub(crate) fn reject_all_pending_reads(&self, ctx: &mut ScopedContext, reason: JSValue) {
        let pending_read = match &self.kind {
            ReadableControllerKind::Default { pending_read, .. } => pending_read,
            ReadableControllerKind::Byte { .. } => return,
        };
        let reads = pending_read.borrow_mut().drain(..).collect::<Vec<_>>();
        for mut p in reads {
            p.reject(ctx, reason);
        }
    }

    /// ReadableStreamDefaultControllerShouldCallPull: pull when the stream
    /// can still produce data, the queue is empty, and either the desired size
    /// is positive or the stream has a pending read (consumer demand).
    /// aike models "started" via `call_pull` itself, so it is not checked here.
    pub(crate) fn should_call_pull(&self, ctx: &mut ScopedContext) -> Result<bool, i32> {
        if self.close_requested() {
            return Ok(false);
        }
        let readable = match self.stream {
            Some(rs) => match rs.payload() {
                Some(sp) => sp.state == RsState::Readable,
                None => false,
            },
            None => false,
        };
        if !readable {
            return Ok(false);
        }
        if !self.queue().borrow().is_empty() {
            return Ok(false);
        }
        Ok(self.desired_size(ctx)? > 0.0)
    }

    /// Ask the underlying source to pull one more chunk. Byte streams do not
    /// use pull-based filling yet, so this is a no-op there.
    pub(crate) fn call_pull(&self, ctx: &mut ScopedContext) -> Result<(), i32> {
        let (pulling, started, pull_again, controller) = match &self.kind {
            ReadableControllerKind::Default {
                pulling,
                started,
                pull_again,
                controller,
                ..
            } => (pulling, started, pull_again, controller),
            ReadableControllerKind::Byte { .. } => return Ok(()),
        };

        // Spec: pull() never runs before start() completes. aike approximates
        // this by treating `started` as set once pulling begins.
        if !started.get() {
            started.set(true);
        }
        if pulling.get() {
            pull_again.set(true);
            return Ok(());
        }

        pulling.set(true);

        let pull_fn = self.pull_algorithm;
        if !pull_fn.is_callable() {
            // No pull callback (push-style source) — nothing to ask for.
            pulling.set(false);
            return Ok(());
        }

        let controller_val =
            controller.ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;

        // Spec: if the underlying source's pull() throws, error the stream with
        // that value (which rejects any pending reads) rather than swallowing it.
        let pull_res = pull_fn.call(
            ctx,
            self.underlying_source,
            &[JSValue::from(controller_val)],
        );
        if pull_res.is_err() {
            let err = ctx.take_exception().to_value(ctx);
            pulling.set(false);
            self.reject_all_pending_reads(ctx, err);
            self.queue.borrow_mut().clear();
            self.queue_total_size.set(0.0);
            if let Some(mut stream_val) = self.stream {
                stream_val
                    .payload_mut()
                    .unwrap()
                    .settle_state(ctx, RsState::Errored, Some(err));
            }
            // For tee branches, propagate the pull error so the caller
            // (tee_do_pull → tee_pull) surfaces it via tee_pull_rejected,
            // which errors both branches after buffered data is consumed.
            let in_tee = if let ReadableControllerKind::Default { in_tee, .. } = &self.kind {
                in_tee.get()
            } else {
                false
            };
            return if in_tee {
                Err(ctx.throw_internal_error("pull() threw"))
            } else {
                Ok(())
            };
        }

        // Capture the pull result before clearing pulling, so we can check
        // whether pull() returned a Thenable (promise).
        let pull_val = pull_res?;

        pulling.set(false);

        if pull_again.get() {
            pull_again.set(false);
            // Spec CallPullIfNeeded step 7: "Upon fulfillment of pullPromise".
            // When pull() returns a non-thenable (synchronous completion,
            // e.g. undefined), the fulfillment is immediate — run the
            // follow-up pull synchronously.  When pull() returns a Thenable,
            // defer to a microtask so the promise can settle first.
            let is_thenable = pull_val.is_object()
                && pull_val
                    .get_property_str(ctx, "then")
                    .map(|t| t.is_callable())
                    .unwrap_or(false);
            if is_thenable {
                let (mut ready, _rr) = Promise::create(ctx)?;
                let data = [JSValue::from(controller_val)].into();
                let follow_up = ctx
                    .create_native_function_with_data(
                        Callback::Func(controller_refill_pull),
                        "refillPull",
                        0,
                        0,
                        data,
                    )?
                    .into();
                ready.resolve(ctx, JSValue::UNDEFINED);
                ready.perform_then(ctx, follow_up, follow_up)?;
            } else if self.should_call_pull(ctx)? {
                self.call_pull(ctx)?;
            }
        }

        Ok(())
    }

    /// Enqueue a chunk. Per the current spec, a non-readable stream or a
    /// close-requested controller makes this a silent no-op
    /// (`CanCloseOrEnqueue` returns false), not a throw.
    /// For `owning` type streams, the options may contain a `transfer` list.
    pub(crate) fn enqueue(
        &self,
        ctx: &mut ScopedContext,
        chunk: JSValue,
        options: JSValue,
    ) -> JSResult {
        // Handle the `transfer` option for owning-type streams.
        let chunk = if !options.is_undefined_or_null() {
            self.handle_transfer(ctx, chunk, options)?
        } else {
            chunk
        };

        if self.close_requested.get() || !self.can_close_or_enqueue() {
            return Ok(JSValue::UNDEFINED);
        }

        match &self.kind {
            ReadableControllerKind::Default { .. } => {
                let resolved = self.resolve_pending_read(ctx, chunk, false);
                if resolved {
                    // A pending read consumed this chunk and the queue is still
                    // below the high water mark — pull again to refill.
                    if self.should_call_pull(ctx)? {
                        self.call_pull(ctx)?;
                    }
                    return Ok(JSValue::UNDEFINED);
                }

                // No pending read: compute the chunk's size via the strategy's
                // size algorithm. A throwing size() errors the stream and
                // re-throws; an invalid size (NaN/±Infinity/negative) is a
                // RangeError that also errors the stream.
                let size = if self.size_algorithm.is_callable() {
                    match self.size_algorithm.call(ctx, JSValue::UNDEFINED, &[chunk]) {
                        Ok(v) => v.to_number(ctx)?.to_f64(),
                        Err(_) => {
                            let e = ctx.take_exception().to_value(ctx);
                            self.error(ctx, e)?;
                            // Re-throw the same value so `enqueue()` surfaces it.
                            ctx.throw(Exception::Error(e));
                            return Err(-1);
                        }
                    }
                } else {
                    1.0
                };
                if size.is_nan() || size.is_infinite() || size < 0.0 {
                    let e = ctx
                        .create_error_object(
                            ErrorKind::Range,
                            "strategy.size must return a finite non-negative number",
                            false,
                        )
                        .ok_or(-1)?;
                    let e = e.into();
                    self.error(ctx, e)?;
                    ctx.throw(Exception::Error(e));
                    return Err(-1);
                }

                self.queue.borrow_mut().push(QueueEntry { chunk, size });
                self.queue_total_size
                    .set(self.queue_total_size.get() + size);
                Ok(JSValue::UNDEFINED)
            }
            ReadableControllerKind::Byte { .. } => {
                self.queue
                    .borrow_mut()
                    .push(QueueEntry { chunk, size: 1.0 });
                self.queue_total_size.set(self.queue_total_size.get() + 1.0);
                Ok(JSValue::UNDEFINED)
            }
        }
    }

    /// Handle the `transfer` option for owning-type streams.
    /// Neuters each buffer in the transfer list via `ArrayBuffer.prototype.transfer()`,
    /// then clones the chunk so it references the transferred data.
    fn handle_transfer(
        &self,
        ctx: &mut ScopedContext,
        chunk: JSValue,
        options: JSValue,
    ) -> Result<JSValue, i32> {
        // Get the transfer list from options. If the getter throws, propagate.
        let transfer_list = match options.as_object() {
            Some(o) => match o.get_property_str(ctx, "transfer") {
                Ok(v) => v,
                Err(e) => return Err(e),
            },
            None => JSValue::UNDEFINED,
        };

        if transfer_list.is_undefined_or_null() {
            return Ok(chunk);
        }

        // Transfer is only allowed for owning-type streams.
        let is_owning = if let ReadableControllerKind::Default { owning, .. } = &self.kind {
            owning.get()
        } else {
            false
        };
        if !is_owning {
            return Err(ctx.throw_type_error(
                "Transfer is only allowed for ReadableStream with type 'owning'",
            ));
        }

        // Convert to array and iterate.
        let len_val = transfer_list.get_property_str(ctx, "length")?;
        let len = len_val.to_number(ctx)?.to_f64() as i32;
        if len <= 0 {
            return Ok(chunk);
        }

        // Build a map: original buffer → transferred buffer.
        let mut replacements: Vec<(JSValue, JSValue)> = Vec::new();
        for i in 0..len {
            let buf = match transfer_list.get_property_u32(ctx, i as u32) {
                Ok(v) => v,
                Err(_) => {
                    ctx.take_exception();
                    continue;
                }
            };
            if buf.is_undefined_or_null() {
                continue;
            }
            // Call ArrayBuffer.prototype.transfer() to neuter the original
            // and get a new buffer with the same data.
            let transfer_fn = match buf.get_property_str(ctx, "transfer") {
                Ok(v) => v,
                Err(_) => {
                    ctx.take_exception();
                    continue;
                }
            };
            if !transfer_fn.is_callable() {
                continue;
            }
            let new_buf = match transfer_fn.call(ctx, buf, &[]) {
                Ok(v) => v,
                Err(_) => {
                    ctx.take_exception();
                    continue;
                }
            };
            replacements.push((buf, new_buf));
        }

        if replacements.is_empty() {
            return Ok(chunk);
        }

        // For TypedArrays: create a new TypedArray backed by the transferred buffer.
        // For plain ArrayBuffers: return the transferred buffer directly.
        let is_ta = chunk
            .get_property_str(ctx, "byteLength")
            .map(|v| !v.is_undefined())
            .unwrap_or(false)
            && chunk
                .get_property_str(ctx, "buffer")
                .map(|v| v.is_object())
                .unwrap_or(false);

        if is_ta {
            // TypedArray: create a new instance with the transferred buffer.
            let constructor = chunk.get_property_str(ctx, "constructor")?;
            if constructor.is_callable() {
                // Find the replacement buffer for this TypedArray's buffer.
                let orig_buf = chunk.get_property_str(ctx, "buffer")?;
                if let Some((_, new_buf)) = replacements.iter().find(|(old, _)| *old == orig_buf) {
                    let byte_offset = chunk
                        .get_property_str(ctx, "byteOffset")
                        .ok()
                        .map(|v| v.to_number(ctx).ok().map(|n| n.to_f64() as u32))
                        .flatten()
                        .unwrap_or(0);
                    let byte_length = chunk
                        .get_property_str(ctx, "byteLength")
                        .ok()
                        .map(|v| v.to_number(ctx).ok().map(|n| n.to_f64() as u32))
                        .flatten()
                        .unwrap_or(0);
                    // TypedArray(buffer, byteOffset, byteLength)
                    return constructor.call(
                        ctx,
                        JSValue::UNDEFINED,
                        &[*new_buf, JSValue::from(byte_offset as f64), JSValue::from(byte_length as f64)],
                    );
                }
            }
            // Fallback: return chunk as-is (transfer didn't match).
            return Ok(chunk);
        }

        // Plain ArrayBuffer or other: return the transferred buffer.
        if let Some((_, new_buf)) = replacements.first() {
            return Ok(*new_buf);
        }

        Ok(chunk)
    }

    /// Spec `ReadableStreamDefaultControllerCanCloseOrEnqueue`: false once the
    /// stream has left the readable state or close has been requested.
    pub(crate) fn can_close_or_enqueue(&self) -> bool {
        if self.close_requested.get() {
            return false;
        }
        let readable = match self.stream {
            Some(rs) => match rs.payload() {
                Some(sp) => sp.state == RsState::Readable,
                None => false,
            },
            None => false,
        };
        readable
    }

    /// Close the controlled stream. Per WHATWG
    /// `ReadableStreamDefaultControllerClose` step 1, throw a TypeError when
    /// the stream is not readable or close was already requested.
    pub(crate) fn close(&self, ctx: &mut ScopedContext) -> Result<(), i32> {
        // Step 1: If ! ReadableStreamDefaultControllerCanCloseOrEnqueue is false,
        // throw a TypeError exception.
        let readable = match self.stream {
            Some(rs) => match rs.payload() {
                Some(sp) => sp.state == RsState::Readable,
                None => false,
            },
            None => false,
        };
        if !readable {
            return Err(ctx.throw_type_error("Stream is not readable"));
        }
        if self.close_requested.get() {
            return Err(ctx.throw_type_error("Stream has already been closed"));
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

    /// Error the controlled stream with `e`. Per WHATWG
    /// `ReadableStreamDefaultControllerError`, the guard is
    /// `stream.[[state]]` only (not `CanCloseOrEnqueue`):
    ///   1. Let stream be controller.[[stream]].
    ///   2. If stream.[[state]] is not "readable", return.
    /// This means `controller.error()` still works after
    /// `controller.terminate()` as long as the readable side hasn't
    /// transitioned away from "readable" (i.e. the queue was non-empty
    /// when terminate closed it).
    pub(crate) fn error(&self, ctx: &mut ScopedContext, e: JSValue) -> Result<(), i32> {
        let is_readable = match self.stream {
            Some(rs) => match rs.payload() {
                Some(sp) => sp.state == RsState::Readable,
                None => false,
            },
            None => false,
        };
        if !is_readable {
            return Ok(());
        }

        match &self.kind {
            ReadableControllerKind::Default { .. } => {
                self.reject_all_pending_reads(ctx, e);
                self.queue.borrow_mut().clear();
                self.queue_total_size.set(0.0);
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

    /// Spec: errored -> null (signalled as `f64::NAN`), closed -> 0, else
    /// highWaterMark - queueTotalSize. The result can be negative when the
    /// queue is over-full; that is the spec-mandated backpressure signal.
    /// A NaN highWaterMark is impossible (rejected at extraction), so NaN is a
    /// safe sentinel for the errored state.
    pub(crate) fn desired_size(&self, _: &mut ScopedContext) -> Result<f64, i32> {
        if let Some(s) = self.stream
            && let Some(sp) = s.payload()
        {
            match sp.state {
                // Readable streams never enter the Erroring state.
                RsState::Errored | RsState::Erroring => return Ok(f64::NAN),
                RsState::Closed => return Ok(0.0),
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

// Follow-up pull scheduled by `call_pull` when a concurrent pull was requested
// while the source was already pulling (the `pullAgain` flag). Runs as a promise
// reaction so a queue drained by a read is refilled before later await steps.
fn controller_refill_pull(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let controller_val = cc.data(0);
    let cp = JSReadableStreamDefaultController::payload_of(&controller_val)
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;
    // Spec `CallPullIfNeeded`: the pullAgain follow-up runs through
    // ShouldCallPull, so a stream that closed/errored meanwhile does not pull
    // again.
    if cp.should_call_pull(ctx)? {
        cp.call_pull(ctx)?;
    }
    Ok(JSValue::UNDEFINED)
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
