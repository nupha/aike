// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! ReadableStream `tee()`: split one source into two branches that both receive
//! every chunk. Shared on-demand pull state and the pump callbacks live here.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use zuqe::{
    Array, CallContext, Callback, JSResult, JSValue, JobState, ScopedContext,
    builtins::promise::Promise,
};

use super::{
    JSReadableStream, JSReadableStreamDefaultController, ReadableControllerPayload,
    ReadableStreamPayload, RsState, reader::JSReadableStreamDefaultReader,
};

/// Shared state for a `tee()` split: one source reader feeding two branch
/// controllers on demand. `pulling` guards against concurrent pulls from both
/// branches; `done` stops pulling once the source finishes.
struct TeeShared {
    reader: JSReadableStreamDefaultReader,
    c1: JSReadableStreamDefaultController,
    c2: JSReadableStreamDefaultController,
    pulling: Cell<bool>,
    done: Cell<bool>,
    /// Branch cancel tracking (per spec the source is only cancelled once both
    /// branches are cancelled). Every branch cancel returns the shared
    /// `cancel_promise`, which settles only when the underlying source cancel
    /// completes — so a pipeTo abort on one branch waits for the other branch.
    cancel1: Cell<bool>,
    cancel2: Cell<bool>,
    cancel_started: Cell<bool>,
    cancel_reason: RefCell<Option<JSValue>>,
    cancel_promise: RefCell<Option<Promise>>,
}

thread_local! {
    static TEE_STREAMS: RefCell<HashMap<u32, Rc<TeeShared>>> = RefCell::new(HashMap::new());
    static NEXT_TEE_ID: Cell<u32> = const { Cell::new(1) };
}

impl JSReadableStreamDefaultReader {
    /// Tee this reader's stream into two branches.
    ///
    /// Consumes the source reader's `closed` rejection (created by getReader)
    /// so an errored source does not leak an unhandled rejection from this
    /// internal reader, then builds two branch streams each driven by the
    /// shared on-demand tee pull, registers the shared state, and returns the
    /// two branch streams.
    pub(crate) fn tee(&mut self, ctx: &mut ScopedContext) -> Result<[JSReadableStream; 2], i32> {
        // Consume the source reader's `closed` rejection (created by getReader) so an
        // errored source does not leak an unhandled rejection from this internal reader.
        if let Some(mut closed_p) = self.payload_mut().and_then(|p| p.closed_promise) {
            let noop = ctx
                .create_native_function(
                    zuqe::Callback::Func(|_, _| Ok(JSValue::UNDEFINED)),
                    "teeIgnore",
                    0,
                    0,
                )?
                .into();
            closed_p.then(ctx, noop, noop)?;
        }

        // Each branch's underlying source `pull` is the shared on-demand tee pull,
        // keyed by a tee id that resolves to the shared state (source reader + both
        // branch controllers). Pull only fires when a branch actually needs data, so
        // an infinite source is never drained unless a branch asks for it.
        let tee_id = NEXT_TEE_ID.with(|c| {
            let id = c.get();
            c.set(id + 1);
            id
        });

        // Create two branch streams, each with its own default controller whose
        // underlying-source pull drives the shared tee pull.
        let make_branch = |ctx: &mut ScopedContext, branch: u32| -> Result<
            (JSReadableStream, JSReadableStreamDefaultController),
            i32,
        > {
            let pull_fn = ctx
                .create_native_function_with_data(
                    Callback::Func(tee_pull),
                    "teePull",
                    1,
                    0,
                    [JSValue::from(tee_id)].into(),
                )?
                .into();
            let cancel_fn = ctx
                .create_native_function_with_data(
                    Callback::Func(tee_cancel),
                    "teeCancel",
                    1,
                    0,
                    [JSValue::from(tee_id), JSValue::from(branch)].into(),
                )?
                .into();
            let mut source = ctx.create_base_object()?;
            source.define_property_value_str(ctx, "pull", pull_fn, zuqe::JS_PROP_C_W_E)?;
            source.define_property_value_str(ctx, "cancel", cancel_fn, zuqe::JS_PROP_C_W_E)?;

            let mut stream2 = JSReadableStream::new_instance(
                ctx,
                JSValue::UNDEFINED,
                ReadableStreamPayload::new(),
            )?;
            let mut controller = JSReadableStreamDefaultController::new_instance(
                ctx,
                JSValue::UNDEFINED,
                ReadableControllerPayload::new_default(JSValue::from(source), Some(stream2), 1.0),
            )?;

            // Wire the controller's self-handle (call_controller_pull needs it).
            let handle = controller;
            controller
                .payload_mut()
                .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
                .set_controller(Some(handle));
            // Register the branch's pump pull/cancel callbacks (captured once).
            controller
                .payload_mut()
                .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
                .set_algorithms(pull_fn, cancel_fn, JSValue::UNDEFINED);
            stream2.payload_mut().unwrap().controller = Some(controller);

            Ok((stream2, controller))
        };

        let (b1, c1) = make_branch(ctx, 0)?;
        let (b2, c2) = make_branch(ctx, 1)?;

        let cancel_promise = Promise::create(ctx)?.0;
        let shared = Rc::new(TeeShared {
            reader: *self,
            c1,
            c2,
            pulling: Cell::new(false),
            done: Cell::new(false),
            cancel1: Cell::new(false),
            cancel2: Cell::new(false),
            cancel_started: Cell::new(false),
            cancel_reason: RefCell::new(None),
            cancel_promise: RefCell::new(Some(cancel_promise)),
        });
        TEE_STREAMS.with(|map| map.borrow_mut().insert(tee_id, shared));

        Ok([b1, b2])
    }
}

// Branch cancel: per the spec, cancelling one tee branch must not cancel the
// underlying source (or settle the cancel promise) until the *other* branch is
// also cancelled. Both branches return the same shared promise, which resolves
// once the real source cancel completes. This is what makes a pipeTo abort on
// one branch wait for the sibling branch.
fn tee_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_u32();
    let branch = cc.data(1).to_number(ctx)?.to_u32();
    let reason = cc.arg(0);
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };

    if branch == 0 {
        shared.cancel1.set(true);
    } else {
        shared.cancel2.set(true);
    }

    // Composite reason: the shared reason if both cancels agree, else an
    // AggregateError (spec: name "Multiple errors").
    let mut cr = shared.cancel_reason.borrow_mut();
    let composite = match &*cr {
        None => reason,
        Some(prev) if *prev == reason => reason,
        Some(prev) => tee_aggregate_error(ctx, *prev, reason)?,
    };
    *cr = Some(composite);
    drop(cr);

    if shared.cancel1.get() && shared.cancel2.get() && !shared.cancel_started.get() {
        shared.cancel_started.set(true);
        let composite = shared.cancel_reason.borrow().unwrap();
        // Cancel the real source (via the tee reader); mirror its settlement
        // into the shared cancel promise both branches already returned.
        let reader = shared.reader;
        let mut src_cancel = reader
            .payload()
            .ok_or_else(|| ctx.throw_internal_error("tee reader released"))?
            .cancel(ctx, composite)?;
        let promise_val = JSValue::from(shared.cancel_promise.borrow().unwrap());
        let ok_fn = ctx
            .create_native_function_with_data(
                Callback::Func(tee_cancel_settle),
                "teeCancelOk",
                0,
                0,
                [promise_val].into(),
            )?
            .into();
        let err_fn = ctx
            .create_native_function_with_data(
                Callback::Func(tee_cancel_settle),
                "teeCancelErr",
                1,
                1,
                [promise_val].into(),
            )?
            .into();
        src_cancel.then(ctx, ok_fn, err_fn)?;
    }

    match shared.cancel_promise.borrow().as_ref() {
        Some(p) => Ok(JSValue::from(*p)),
        None => Ok(JSValue::UNDEFINED),
    }
}

// Mirror the source-cancel settlement into the shared branch cancel promise.
// magic 0 = fulfillment, 1 = rejection (the callback is shared by both paths).
fn tee_cancel_settle(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut p = match Promise::try_from(cc.data(0)) {
        Ok(p) => p,
        Err(_) => return Ok(JSValue::UNDEFINED),
    };
    if cc.magic() == 1 {
        p.reject(ctx, cc.arg(0));
    } else {
        p.resolve(ctx, JSValue::UNDEFINED);
    }
    Ok(JSValue::UNDEFINED)
}

// Build an AggregateError composite reason, falling back to a plain error if
// the AggregateError constructor is unavailable.
fn tee_aggregate_error(
    ctx: &mut ScopedContext,
    a: JSValue,
    b: JSValue,
) -> Result<JSValue, i32> {
    if let Ok(agg) = ctx.global_obj().get_property_str(ctx, "AggregateError") {
        if agg.is_callable() {
            let arr = Array::from_values(ctx, vec![a, b])?;
            let msg = ctx.create_string_from_utf8(b"Multiple errors".to_vec())?;
            if let Ok(res) = agg.call_ctor(ctx, &[arr.into(), JSValue::make_string(msg)]) {
                return Ok(res);
            }
        }
    }
    let msg = ctx.create_string_from_utf8(b"Multiple errors".to_vec())?;
    Ok(JSValue::make_string(msg))
}

// ReadableStream.prototype.tee()
// ============================================================================
// Returns [branch1, branch2], two ReadableStreams that both receive every chunk
// of `this`. The source is locked with a default reader, and a read-ahead pump
// pulls chunks and enqueues them into both branches; source error propagates to
// both; source close closes both.
//
// Callback `data` layout for the tee pump:
//   [0] = source reader, [1] = reader.read(), [2] = branch1 controller,
//   [3] = branch2 controller
pub(crate) fn js_readable_stream_tee(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let mut stream = this
        .as_native_object::<JSReadableStream>()
        .ok_or_else(|| ctx.throw_type_error("not a ReadableStream"))?;

    let mut reader = stream.acquire_reader(ctx, false)?;

    // `tee` returns the two branch streams directly.
    let branches = reader.tee(ctx)?;

    // The branches are exposed in a new array.
    Array::from_values(ctx, vec![branches[0].into(), branches[1].into()]).map(JSValue::from)
}

// On-demand tee pull: driven by a branch controller's `pull` (fired only when a
// branch read needs data). Fetches at most one source chunk and enqueues it into
// both live branches. A shared `pulling` guard prevents concurrent pulls from
// both branches; a shared `done` stops pulling after the source finishes.
fn tee_pull(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_u32();
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };
    tee_do_pull(ctx, tee_id, &shared)
}

// Source read -> {done, value}: enqueue into both live branches and clear the
// pulling guard. Close both when the source is done.
fn tee_pull_fulfilled(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_u32();
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };
    shared.pulling.set(false);
    let result = cc.arg(0);
    let done = result
        .get_property_str(ctx, "done")?
        .to_bool()
        .unwrap_or(false);
    if done {
        shared.done.set(true);
        for c in [shared.c1, shared.c2] {
            if !tee_branch_is_final(ctx, c.into()) {
                c.payload().unwrap().close(ctx)?;
            }
        }
        TEE_STREAMS.with(|m| m.borrow_mut().remove(&tee_id));
        return Ok(JSValue::UNDEFINED);
    }

    let value = result.get_property_str(ctx, "value")?;
    for c in [shared.c1, shared.c2] {
        if !tee_branch_is_final(ctx, c.into()) {
            c.invoke_str(ctx, "enqueue", &[value])?;
        }
    }

    // If any branch still has an outstanding read waiting for data, keep pulling
    // (concurrent reads on a branch produce multiple pending reads; one source
    // chunk only satisfies one of them).
    let need_more = [shared.c1, shared.c2].iter().any(|&c| {
        !tee_branch_is_final(ctx, c.into())
            && JSReadableStreamDefaultController::payload_of(&c.into())
                .is_some_and(|cp| !cp.pending_read().borrow().is_empty())
    });

    if need_more {
        let id = tee_id;
        ctx.queue_job(
            move |cx| {
                if let Some(shared) = TEE_STREAMS.with(|m| m.borrow().get(&id).cloned()) {
                    tee_do_pull(cx, id, &shared)?;
                }
                Ok(JobState::Done)
            },
            (),
        );
    }

    Ok(JSValue::UNDEFINED)
}

// Perform the shared pull: read one source chunk and fan it out to both branches.
fn tee_do_pull(ctx: &mut ScopedContext, tee_id: u32, shared: &TeeShared) -> JSResult {
    if shared.pulling.get() || shared.done.get() {
        return Ok(JSValue::UNDEFINED);
    }
    shared.pulling.set(true);

    let p = shared.reader.payload().unwrap();
    let mut promise = p.read(ctx, None)?;

    let step = ctx
        .create_native_function_with_data(
            Callback::Func(tee_pull_fulfilled),
            "teePullOk",
            1,
            0,
            [JSValue::from(tee_id)].into(),
        )?
        .into();
    let fail = ctx
        .create_native_function_with_data(
            Callback::Func(tee_pull_rejected),
            "teePullErr",
            1,
            0,
            [JSValue::from(tee_id)].into(),
        )?
        .into();
    promise.then(ctx, step, fail)?;

    Ok(JSValue::UNDEFINED)
}

// Source read rejected: error both branches and clear the pulling guard.
fn tee_pull_rejected(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tee_id = cc.data(0).to_number(ctx)?.to_u32();
    let shared = TEE_STREAMS.with(|m| m.borrow().get(&tee_id).cloned());
    let Some(shared) = shared else {
        return Ok(JSValue::UNDEFINED);
    };
    shared.pulling.set(false);
    shared.done.set(true);
    let reason = cc.arg(0);
    for c in [shared.c1, shared.c2] {
        if !tee_branch_is_final(ctx, c.into()) {
            c.invoke_str(ctx, "error", &[reason])?;
        }
    }
    TEE_STREAMS.with(|m| m.borrow_mut().remove(&tee_id));
    Ok(JSValue::UNDEFINED)
}

// A branch controller is "final" (errored or closed): skip enqueue/close/error.
pub(crate) fn tee_branch_is_final(_ctx: &mut ScopedContext, c: JSValue) -> bool {
    let Some(cp) = JSReadableStreamDefaultController::payload_of(&c) else {
        return true;
    };
    let Some(s) = cp.stream() else {
        return true;
    };
    let Some(sp) = s.payload() else {
        return true;
    };
    matches!(sp.state, RsState::Errored | RsState::Closed)
}
