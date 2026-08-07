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

use crate::whatwg::stream::readable_stream::get_readable_stream_payload;

use super::{
    JSReadableStream, ReadableStreamPayload, RsState,
    readable_stream_default_controller::{
        JSReadableStreamDefaultController, ReadableStreamDefaultControllerPayload,
    },
    readable_stream_default_reader::JSReadableStreamDefaultReader,
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
}

thread_local! {
    static TEE_STREAMS: RefCell<HashMap<u32, Rc<TeeShared>>> = RefCell::new(HashMap::new());
    static NEXT_TEE_ID: Cell<u32> = const { Cell::new(1) };
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

    // if p.reader.is_some() {
    //     return Err(ctx.throw_type_error("ReadableStream is already locked"));
    // }
    // let reader = this
    //     .get_property_str(ctx, "getReader")?
    //     .call(ctx, this, &[])?;

    let reader = stream.acquire_reader(ctx, false)?;
    let p = get_readable_stream_payload(ctx, &this)?;

    // Consume the source reader's `closed` rejection (created by getReader) so an
    // errored source does not leak an unhandled rejection from this internal reader.

    if let Ok(closed) = reader.get_property_str(ctx, "closed")
        && let Ok(mut closed_p) = Promise::try_from(closed)
    {
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
    let make_branch = |ctx: &mut ScopedContext| -> Result<(JSReadableStream,JSReadableStreamDefaultController ), i32> {
        let pull_fn = ctx
            .create_native_function_with_data(
                Callback::Func(tee_pull),
                "teePull",
                1,
                0,
                [JSValue::from(tee_id)].into(),
            )?
            .into();
        let mut source = ctx.create_base_object()?;
        source.define_property_value_str(ctx, "pull", pull_fn, zuqe::JS_PROP_C_W_E)?;

        let mut stream =
            JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
        let mut controller = JSReadableStreamDefaultController::new_instance(
            ctx,
            JSValue::UNDEFINED,
            ReadableStreamDefaultControllerPayload::new(JSValue::from(source), Some(stream), 1.0),
        )?;

        // Wire the controller's self-handle (call_controller_pull needs it).
        let handle = controller;
        controller
            .payload_mut()
            .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?
            .controller = Some(handle);
        stream.payload_mut().unwrap().controller = Some(controller);

        Ok(( stream , controller) )
    };

    let (branch1_val, c1) = make_branch(ctx)?;
    let (branch2_val, c2) = make_branch(ctx)?;

    let shared = Rc::new(TeeShared {
        reader,
        c1,
        c2,
        pulling: Cell::new(false),
        done: Cell::new(false),
    });
    TEE_STREAMS.with(|map| map.borrow_mut().insert(tee_id, shared));

    // The branches are exposed in a new array.
    Array::from_values(ctx, vec![branch1_val.into(), branch2_val.into()]).map(JSValue::from)
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
                c.invoke_str(ctx, "close", &[])?;
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
    let need_more = [shared.c1, shared.c2]
        .iter()
        .any(|&c| !tee_branch_is_final(ctx, c.into()) && tee_has_pending_read(ctx, c.into()));

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

// True when the branch controller has an outstanding pending read waiting for
// a chunk to be enqueued.
fn tee_has_pending_read(_ctx: &mut ScopedContext, c: JSValue) -> bool {
    let Some(cp) = JSReadableStreamDefaultController::payload_of(&c) else {
        return false;
    };
    !cp.pending_read.borrow().is_empty()
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
    let Some(s) = cp.stream else {
        return true;
    };
    let Some(sp) = s.payload() else {
        return true;
    };
    matches!(sp.state, RsState::Errored | RsState::Closed)
}
