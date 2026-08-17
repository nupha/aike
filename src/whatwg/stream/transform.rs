// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! TransformStream, wiring a readable and a writable side together.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::promise::Promise,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::stream::{
        JSReadableStreamDefaultController, ReadableControllerPayload, extract_high_water_mark,
        extract_size_algorithm, readable_controller::get_readable_controller,
        readable::{JSReadableStream, ReadableStreamPayload},
        transform_controller::{JSTransformStreamDefaultController, TransformControllerPayload},
        writable::{JSWritableStream, WritableStreamPayload},
        writable_controller::{JSWritableStreamDefaultController, WritableControllerPayload},
    },
};

// ============================================================================
// TransformStream
// ============================================================================

pub(crate) struct TransformStreamPayload {
    readable: Option<JSReadableStream>,
    writable: Option<JSWritableStream>,
    pub(crate) controller: Option<JSTransformStreamDefaultController>,
    /// WHATWG `[[backpressure]]` — when true, writes must wait for
    /// `[[backpressureChangePromise]]` before calling transform.
    pub(crate) backpressure: bool,
    /// WHATWG `[[backpressureChangePromise]]` — resolves when backpressure
    /// changes (set by `TransformStreamSetBackpressure`).
    pub(crate) backpressure_change_promise: Option<Promise>,
}

impl TransformStreamPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(ref r) = self.readable {
            r.trace(gcx);
        }
        if let Some(ref w) = self.writable {
            w.trace(gcx);
        }
        if let Some(ref c) = self.controller {
            c.trace(gcx);
        }
        if let Some(ref p) = self.backpressure_change_promise {
            p.trace(gcx);
        }
    }
}

const TRANSFORM_STREAM_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("readable", Some(js_transform_stream_readable), None, 0),
    FieldDef::GETSET("writable", Some(js_transform_stream_writable), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "TransformStream",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

def_native_object!(
    "TransformStream",
    JSTransformStream,
    TransformStreamPayload,
    TRANSFORM_STREAM_PROTO,
    Some(ClassCtorDef::new(js_transform_stream_constructor, 1, false).expose_to_global()),
    on_payload_gc: TransformStreamPayload::gc_trace
);

fn js_transform_stream_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let transformer = cc.arg(0);

    // Use existing readable/writable instances, or create an empty transformer object.
    let transformer = if transformer.is_undefined_or_null() {
        ctx.create_base_object().map(JSValue::from)?
    } else {
        transformer
    };

    // Spec §6.2.4 steps 3-4: a defined readableType/writableType is rejected.
    if let Some(tobj) = transformer.as_object() {
        let readable_type = tobj.get_property_str(ctx, "readableType")?;
        if !readable_type.is_undefined() {
            return Err(ctx.throw_range_error("readableType must be undefined"));
        }
        let writable_type = tobj.get_property_str(ctx, "writableType")?;
        if !writable_type.is_undefined() {
            return Err(ctx.throw_range_error("writableType must be undefined"));
        }
    }

    // Per the WHATWG spec the second/third arguments are the writable and
    // readable queuing strategies respectively.
    let writable_strategy = cc.arg(1);
    let readable_strategy = cc.arg(2);
    // Spec §6.2.4: default writable HWM = 1, readable HWM = 0.
    let writable_hwm = extract_high_water_mark(ctx, writable_strategy)?;
    let readable_hwm = extract_high_water_mark(ctx, readable_strategy)?;
    // The readable side uses the readable strategy's `size` algorithm; a
    // throwing size() inside start()/transform() errors the stream (spec
    // `CreateReadableStream` -> `SetUpReadableStreamDefaultController`).
    let readable_size = extract_size_algorithm(ctx, readable_strategy)?;

    // Readable side — created first so the TransformStream can reference it.
    let mut readable =
        JSReadableStream::new_instance(ctx, JSValue::UNDEFINED, ReadableStreamPayload::new())?;
    let mut rc = JSReadableStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        ReadableControllerPayload::new_default(JSValue::UNDEFINED, Some(readable), readable_hwm),
    )?;
    // Set the controller's self-handle so call_pull can find it.
    let rc_handle = rc;
    if let Some(rcp) = rc.payload_mut() {
        rcp.set_controller(Some(rc_handle));
    }
    readable.payload_mut().unwrap().controller.replace(rc);

    // Writable side — use the original transformer as the underlying_sink.
    let mut writable =
        JSWritableStream::new_instance(ctx, JSValue::UNDEFINED, WritableStreamPayload::new())?;
    let wc_payload = WritableControllerPayload::new(
        transformer,
        Some(writable),
        writable_hwm,
        JSValue::UNDEFINED,
    );
    let wcv = JSWritableStreamDefaultController::new_instance(ctx, JSValue::UNDEFINED, wc_payload)?;
    writable.payload_mut().unwrap().controller.replace(wcv);

    // Capture rc and writable again after mutable borrow
    let mut rc = readable.payload().unwrap().controller.unwrap();
    let mut wc = writable.payload().unwrap().controller.unwrap();
    let transformer = if let Some(cp) = wc.payload() {
        cp.underlying_sink
    } else {
        JSValue::UNDEFINED
    };

    // Transform controller — created before the TransformStream instance so we
    // can wire it in.
    let tc = JSTransformStreamDefaultController::new_instance(
        ctx,
        JSValue::UNDEFINED,
        TransformControllerPayload::new(rc, writable, transformer),
    )?;

    // Wire the transform controller into the writable controller.
    if let Some(wcp) = wc.payload_mut() {
        wcp.transform_controller = Some(tc);
    }

    // Create the TransformStream instance with initial backpressure = true
    // (spec §6.4.1 InitializeTransformStream step 10).
    let instance = JSTransformStream::new_instance(
        ctx,
        new_target,
        TransformStreamPayload {
            readable: Some(readable),
            writable: Some(writable),
            controller: Some(tc),
            backpressure: true,
            backpressure_change_promise: None,
        },
    )?;

    // Initialize backpressure (spec §6.4.1 step 10).
    // Use Rc<Cell> shared between all copies of the controller payload
    // (payload() returns &T to heap-allocated storage, so Rc<Cell> works).
    // The promise is stored on BOTH the transform payload (read by the write
    // path) and the controller's Rc<Cell> (read by the backpressure fast path).
    {
        let (new_promise, _rr) = Promise::create(ctx)?;
        let ts_val = JSValue::from(instance);
        if let Some(tsp_mut) = JSTransformStream::payload_mut_of(&mut JSValue::from(ts_val)) {
            tsp_mut.backpressure_change_promise = Some(new_promise);
        }
        if let Some(ctrl_handle) = instance.payload().and_then(|p| p.controller) {
            let mut ctrl = ctrl_handle;
            if let Some(tcp) = ctrl.payload_mut() {
                tcp.backpressure.set(true);
                tcp.backpressure_change_promise.set(Some(new_promise));
            }
        }
    }

    // Wire the [[stream]] back-reference on the controller.
    // tc was moved into the payload, so get it back from the instance.
    let mut tc_from_instance = instance.payload().unwrap().controller.unwrap();
    if let Some(tcp) = tc_from_instance.payload_mut() {
        tcp.stream = Some(instance);
    }

    // Create a wrapper as underlying_sink with a `write` method for the
    // normal writable path (invoke_str_callable finds it).
    {
        let tc_handle = instance
            .payload()
            .and_then(|p| p.controller)
            .unwrap();
        let mut wrapper = ctx.create_base_object()?;
        let write_fn = ctx.create_native_function_with_data(
            zuqe::Callback::Func(ts_sink_write),
            "tsSinkWrite",
            1,
            0,
            [JSValue::from(tc_handle)].into(),
        )?;
        wrapper.define_property_value_str(
            ctx,
            "write",
            JSValue::from(write_fn),
            zuqe::JS_PROP_C_W_E,
        )?;
        // Copy start/close from the original transformer.
        if let Some(tobj) = transformer.as_object() {
            for method in ["start", "close"] {
                if let Ok(v) = tobj.get_property_str(ctx, method) {
                    if !v.is_undefined() {
                        wrapper.define_property_value_str(ctx, method, v, zuqe::JS_PROP_C_W_E)?;
                    }
                }
            }
        }
        // TransformStreamDefaultSinkAbortAlgorithm (spec §6.4.3): abort
        // propagates via readable.cancel(reason) → transformer.cancel(reason),
        // NOT via transformer.abort().
        {
            let ts_val = JSValue::from(instance);
            let abort_fn = ctx.create_native_function_with_data(
                zuqe::Callback::Func(ts_sink_abort),
                "tsSinkAbort",
                1,
                0,
                [ts_val].into(),
            )?;
            wrapper.define_property_value_str(
                ctx,
                "abort",
                JSValue::from(abort_fn),
                zuqe::JS_PROP_C_W_E,
            )?;
        }
        // Update the writable controller's underlying_sink to be the wrapper.
        let ws_val = JSValue::from(writable);
        if let Some(sp) = JSWritableStream::payload_of(&ws_val)
            && let Some(mut ctrl) = sp.controller
            && let Some(wcp) = ctrl.payload_mut()
        {
            wcp.underlying_sink = JSValue::from(wrapper);
        }
    }

    // Wire pull algorithm on the readable controller: TransformStreamDefaultSourcePullAlgorithm
    // (spec §6.4.4). When the readable side needs data, relieve backpressure.
    let pull_fn = {
        let instance_val = JSValue::from(instance);
        ctx.create_native_function_with_data(
            zuqe::Callback::Func(ts_source_pull),
            "tsSourcePull",
            0,
            0,
            [instance_val].into(),
        )?
    };
    // Wire cancel algorithm: TransformStreamDefaultSourceCancelAlgorithm (spec §6.4.4).
    let cancel_fn = {
        let instance_val = JSValue::from(instance);
        ctx.create_native_function_with_data(
            zuqe::Callback::Func(ts_source_cancel),
            "tsSourceCancel",
            1,
            0,
            [instance_val].into(),
        )?
    };
    // Set pull/cancel on the readable controller so they fire on read/cancel.
    if let Some(rcp) = rc.payload_mut() {
        rcp.set_algorithms(pull_fn.into(), cancel_fn.into(), readable_size);
    }

    // Call transformer.start(transformController) if present. A throw propagates
    // via `?` so the construction fails and the caller sees the error.
    if let Some(obj) = transformer.as_object()
        && let Ok(start_fn) = obj.get_property_str(ctx, "start")
        && start_fn.is_callable()
        && let Some(ctrl) = instance.payload().and_then(|p| p.controller)
    {
        start_fn.call(ctx, transformer, &[JSValue::from(ctrl)])?;
    }

    // Schedule a microtask to set started on both readable and writable
    // controllers and trigger pull if appropriate. The streams were created
    // outside js_readable_stream_constructor / js_writable_stream_constructor
    // so the normal start→pull microtasks were never scheduled.
    {
        let rc_handle = rc;
        let ws_handle = writable;
        ctx.queue_job(
            move |cx| {
                // Set started on the readable controller and pull if needed.
                let rc_val = JSValue::from(rc_handle);
                if let Some(rcp) = JSReadableStreamDefaultController::payload_of(&rc_val) {
                    rcp.set_started(true);
                    if rcp.should_call_pull(cx)? {
                        rcp.call_pull(cx)?;
                    }
                }
                // Set started on the writable side and advance the queue.
                if let Some(wsp) = JSWritableStream::payload_mut_of(&mut JSValue::from(ws_handle)) {
                    wsp.started = true;
                    wsp.advance_queue(cx)?;
                    wsp.try_close(cx)?;
                }
                Ok(zuqe::JobState::Done)
            },
            JSValue::from(rc_handle),
        );
    }

    Ok(instance.into())
}

/// TransformStreamDefaultSinkAbortAlgorithm (spec §6.4.3).
/// Abort propagates by calling transformer.cancel() directly, then
/// erroring the readable side with the cancel reason.
fn ts_sink_abort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let reason = cc.arg(0);
    let ts_val = cc.data(0);

    let Some(tsp) = JSTransformStream::payload_of(&ts_val) else {
        return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from);
    };
    let mut controller = match tsp.controller {
        Some(c) => c,
        None => return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from),
    };
    let readable = match tsp.readable {
        Some(r) => r,
        None => return Promise::from_resolve_value(ctx, JSValue::UNDEFINED).map(JSValue::from),
    };

    // If finishPromise is already set, return it.
    if let Some(tcp) = controller.payload() {
        if let Some(fp) = tcp.finish_promise {
            return Ok(JSValue::from(fp));
        }
    }

    // Create finishPromise.
    let (finish_promise, _finish_funcs) = Promise::create(ctx)?;
    if let Some(tcp) = controller.payload_mut() {
        tcp.finish_promise = Some(finish_promise);
    }

    // Call controller.[[cancelAlgorithm]](reason) — i.e. transformer.cancel(reason).
    let cancel_result = if let Some(tcp) = controller.payload()
        && let Some(transformer_obj) = tcp.transformer.as_object()
        && let Ok(cancel_fn) = transformer_obj.get_property_str(ctx, "cancel")
        && cancel_fn.is_callable()
    {
        cancel_fn.call(ctx, tcp.transformer, &[reason])
    } else {
        Ok(JSValue::UNDEFINED)
    };

    // Clear algorithms.
    if let Some(tcp) = controller.payload_mut() {
        tcp.cancel_algorithm = JSValue::UNDEFINED;
        tcp.transform_algorithm = JSValue::UNDEFINED;
        tcp.flush_algorithm = JSValue::UNDEFINED;
    }

    // React to cancel result.
    match cancel_result {
        Ok(v) => {
            if let Ok(mut cancel_promise) = Promise::try_from(v) {
                // Async: chain .then()
                let on_ok = ctx.create_native_function_with_data(
                    zuqe::Callback::Func(ts_sink_abort_ok),
                    "tsSinkAbortOk",
                    1,
                    0,
                    [JSValue::from(controller), readable.into(), reason].into(),
                )?;
                let on_err = ctx.create_native_function_with_data(
                    zuqe::Callback::Func(ts_sink_abort_err),
                    "tsSinkAbortErr",
                    1,
                    0,
                    [JSValue::from(controller), readable.into()].into(),
                )?;
                cancel_promise.then(ctx, on_ok.into(), on_err.into())?;
            } else {
                // Sync non-promise result: treat as fulfilled.
                ts_sink_abort_fulfill(ctx, &mut controller, readable, reason)?;
            }
        }
        Err(_) => {
            let e = ctx.take_exception().to_value(ctx);
            ts_sink_abort_reject(ctx, &mut controller, readable, e)?;
        }
    }

    if let Some(tcp) = controller.payload() {
        if let Some(fp) = tcp.finish_promise {
            return Ok(JSValue::from(fp));
        }
    }
    Ok(JSValue::UNDEFINED)
}

/// Promise reaction: async transformer.cancel() fulfilled (sink abort path).
fn ts_sink_abort_ok(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut controller = cc.data(0)
        .as_native_object::<JSTransformStreamDefaultController>()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;
    let readable = cc.data(1).as_native_object::<JSReadableStream>()
        .ok_or_else(|| ctx.throw_internal_error("invalid readable"))?;
    let reason = cc.data(2);
    ts_sink_abort_fulfill(ctx, &mut controller, readable, reason)
}

/// Promise reaction: async transformer.cancel() rejected (sink abort path).
fn ts_sink_abort_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut controller = cc.data(0)
        .as_native_object::<JSTransformStreamDefaultController>()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;
    let readable = cc.data(1).as_native_object::<JSReadableStream>()
        .ok_or_else(|| ctx.throw_internal_error("invalid readable"))?;
    let error = cc.arg(0);
    ts_sink_abort_reject(ctx, &mut controller, readable, error)
}

/// Fulfilled branch of sink abort: if readable errored → reject
/// finishPromise; otherwise error readable with cancel reason and
/// resolve finishPromise.
fn ts_sink_abort_fulfill(
    ctx: &mut ScopedContext,
    controller: &mut JSTransformStreamDefaultController,
    readable: JSReadableStream,
    reason: JSValue,
) -> JSResult {
    // If readable is errored, reject finishPromise with its storedError.
    if let Some(rp) = readable.payload()
        && rp.state == crate::whatwg::stream::RsState::Errored
    {
        let error = rp.stored_error;
        if let Some(tcp) = controller.payload_mut()
            && let Some(mut fp) = tcp.finish_promise
        {
            fp.reject(ctx, error);
        }
        return Ok(JSValue::UNDEFINED);
    }

    // Error readable with the cancel reason.
    if let Some(tcp) = controller.payload() {
        let rcv = JSValue::from(tcp.readable_controller);
        if let Ok(rcp) = get_readable_controller(ctx, &rcv) {
            rcp.error(ctx, reason)?;
        }
    }
    // Resolve finishPromise.
    if let Some(tcp) = controller.payload_mut()
        && let Some(mut fp) = tcp.finish_promise
    {
        fp.resolve(ctx, JSValue::UNDEFINED);
    }
    Ok(JSValue::UNDEFINED)
}

/// Rejected branch of sink abort: error readable with rejection reason,
/// reject finishPromise.
fn ts_sink_abort_reject(
    ctx: &mut ScopedContext,
    controller: &mut JSTransformStreamDefaultController,
    readable: JSReadableStream,
    error: JSValue,
) -> JSResult {
    if let Some(tcp) = controller.payload() {
        let rcv = JSValue::from(tcp.readable_controller);
        if let Ok(rcp) = get_readable_controller(ctx, &rcv) {
            rcp.error(ctx, error)?;
        }
    }
    if let Some(tcp) = controller.payload_mut()
        && let Some(mut fp) = tcp.finish_promise
    {
        fp.reject(ctx, error);
    }
    Ok(JSValue::UNDEFINED)
}

/// TransformStreamDefaultSinkWriteAlgorithm (spec §6.4.3).
/// Called by the normal writable path's chain_write_result → sink.write().
fn ts_sink_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let chunk = cc.arg(0);
    let tc_handle_val = cc.data(0);

    let Some(tc_handle) = JSTransformStreamDefaultController::try_from(tc_handle_val).ok() else {
        return Ok(JSValue::UNDEFINED);
    };
    let Some(tcp) = tc_handle.payload() else {
        return Ok(JSValue::UNDEFINED);
    };
    let Some(transformer_obj) = tcp.transformer.as_object() else {
        return Ok(JSValue::UNDEFINED);
    };

    let has_transform = transformer_obj
        .get_property_str(ctx, "transform")
        .map(|v| v.is_callable())
        .unwrap_or(false);

    // Spec `TransformStreamDefaultSinkWriteAlgorithm` step 3: while backpressured,
    // the transform (identity or explicit) waits for the backpressure change
    // promise before running.
    let backpressured = tcp.stream
        .and_then(|h| {
            let sv = JSValue::from(h);
            super::transform::JSTransformStream::payload_of(&sv)
                .map(|tsp| tsp.backpressure)
        })
        .unwrap_or(false);

    if backpressured {
        let bp = tcp.stream
            .and_then(|h| {
                let sv = JSValue::from(h);
                super::transform::JSTransformStream::payload_of(&sv)
                    .and_then(|tsp| tsp.backpressure_change_promise)
            });
        if let Some(mut bp) = bp {
            let tc_val = JSValue::from(tc_handle);
            let transformer_val = tcp.transformer;
            let data: Vec<JSValue> = [transformer_val, tc_val, chunk].into();
            let data_ok = data.clone();
            let data_err = data;
            let on_ok = ctx.create_native_function_with_data(
                zuqe::Callback::Func(ts_perform_transform_cb),
                "tsSinkWriteAfterBP",
                0,
                0,
                data_ok.into(),
            )?;
            let on_err = ctx.create_native_function_with_data(
                zuqe::Callback::Func(|_ctx, _cc| Ok(JSValue::UNDEFINED)),
                "tsSinkWriteAfterBPErr",
                1,
                0,
                data_err.into(),
            )?;
            // The write settles with the transform's own outcome: the wrapper
            // promise rejects when the transform (or a terminated writable)
            // fails, not merely when backpressure clears.
            let wrapper = bp.then(ctx, on_ok.into(), on_err.into())?;
            return Ok(wrapper.into());
        }
    }

    // Not backpressured: perform the transform now.
    let data: Vec<JSValue> = [tcp.transformer, JSValue::from(tc_handle), chunk].into();
    ts_perform_transform_values(ctx, data[0], data[1], data[2])
}

/// `TransformStreamDefaultControllerPerformTransform` (spec §6.4.3): for an
/// identity transformer enqueue the chunk on the readable controller; otherwise
/// invoke `transformer.transform(chunk, controller)`.
fn ts_perform_transform_values(
    ctx: &mut ScopedContext,
    transformer_val: JSValue,
    tc_val: JSValue,
    chunk: JSValue,
) -> JSResult {
    let Some(obj) = transformer_val.as_object() else {
        return Ok(JSValue::UNDEFINED);
    };
    let transform_fn = obj.get_property_str(ctx, "transform")?;
    if transform_fn.is_callable() {
        return transform_fn.call(ctx, transformer_val, &[chunk, tc_val]);
    }
    // Identity transform: forward the chunk to the readable side.
    let enqueue_fn = tc_val.get_property_str(ctx, "enqueue")?;
    enqueue_fn.call(ctx, tc_val, &[chunk])
}

// Backpressure-gated write reaction: run the pending transform.
// `cc.data`: [0] = transformer, [1] = transform controller, [2] = chunk.
fn ts_perform_transform_cb(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tc_val = cc.data(1);
    // Spec `TransformStreamDefaultSinkWriteAlgorithm` step 3.1.3: once
    // backpressure clears, an erroring writable rejects the pending write with
    // its stored error (e.g. after `controller.terminate()`).
    if let Some(tc_handle) = JSTransformStreamDefaultController::try_from(tc_val).ok()
        && let Some(tcp) = tc_handle.payload()
        && let Some(wsp) = tcp.writable_stream.payload()
        && (wsp.state == crate::whatwg::stream::RsState::Erroring
            || wsp.state == crate::whatwg::stream::RsState::Errored)
    {
        let e = wsp.stored_error;
        ctx.throw(zuqe::Exception::Error(e));
        return Err(-1);
    }
    ts_perform_transform_values(ctx, cc.data(0), cc.data(1), cc.data(2))
}

/// TransformStreamDefaultSourcePullAlgorithm (spec §6.4.4).
/// When the readable side needs data (consumer reads), relieve backpressure.
fn ts_source_pull(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let stream_val = cc.data(0);
    // The stream parameter here is the TransformStream (we passed it as data).
    if let Some(tsp) = JSTransformStream::payload_of(&stream_val) {
        // Backpressure should be true when pull is called.
        // TransformStreamSetBackpressure(stream, false).
        transform_stream_set_backpressure(ctx, &stream_val, false)?;
    }
    // Return the backpressureChangePromise per spec step 3.
    if let Some(tsp) = JSTransformStream::payload_of(&stream_val) {
        if let Some(p) = tsp.backpressure_change_promise {
            return Ok(JSValue::from(p));
        }
    }
    Ok(JSValue::UNDEFINED)
}

/// TransformStreamDefaultSourceCancelAlgorithm (spec §6.4.4).
/// When the readable side is cancelled, call transformer.cancel(reason),
/// then error the writable side and unblock any pending write.
fn ts_source_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let stream_val = cc.data(0);
    let reason = cc.arg(0);

    let Some(tsp) = JSTransformStream::payload_of(&stream_val) else {
        return Ok(JSValue::UNDEFINED);
    };
    let mut controller = match tsp.controller {
        Some(c) => c,
        None => return Ok(JSValue::UNDEFINED),
    };
    let writable = match tsp.writable {
        Some(w) => w,
        None => return Ok(JSValue::UNDEFINED),
    };

    // If finishPromise is already set, return it (idempotent cancel).
    if let Some(tcp) = controller.payload() {
        if let Some(fp) = tcp.finish_promise {
            return Ok(JSValue::from(fp));
        }
    }

    // Create finishPromise (spec step 4).
    let (finish_promise, _finish_funcs) = Promise::create(ctx)?;

    // Store finishPromise on the controller.
    if let Some(tcp) = controller.payload_mut() {
        tcp.finish_promise = Some(finish_promise);
    }

    // Call transformer.cancel(reason) (spec step 5).
    let cancel_result = if let Some(tcp) = controller.payload()
        && let Some(transformer_obj) = tcp.transformer.as_object()
        && let Ok(cancel_fn) = transformer_obj.get_property_str(ctx, "cancel")
        && cancel_fn.is_callable()
    {
        cancel_fn.call(ctx, tcp.transformer, &[reason])
    } else {
        Ok(JSValue::UNDEFINED)
    };

    // Clear algorithms (spec step 6).
    if let Some(tcp) = controller.payload_mut() {
        tcp.cancel_algorithm = JSValue::UNDEFINED;
        tcp.transform_algorithm = JSValue::UNDEFINED;
        tcp.flush_algorithm = JSValue::UNDEFINED;
    }

    // React to cancel result (spec step 7).
    match cancel_result {
        Ok(v) => {
            // Check if the result is a promise (async cancel).
            if let Ok(mut cancel_promise) = Promise::try_from(v) {
                // Async cancel: chain .then() to react on settlement.
                let on_ok = ctx.create_native_function_with_data(
                    zuqe::Callback::Func(ts_source_cancel_ok),
                    "tsSourceCancelOk",
                    1,
                    0,
                    [stream_val, JSValue::from(controller), reason].into(),
                )?;
                let on_err = ctx.create_native_function_with_data(
                    zuqe::Callback::Func(ts_source_cancel_err),
                    "tsSourceCancelErr",
                    1,
                    0,
                    [stream_val, JSValue::from(controller)].into(),
                )?;
                cancel_promise.then(ctx, on_ok.into(), on_err.into())?;
            } else {
                // Sync cancel (non-promise result): treat as fulfilled.
                ts_cancel_fulfill(ctx, &stream_val, &mut controller, writable, reason)?;
            }
        }
        Err(_) => {
            let e = ctx.take_exception().to_value(ctx);
            // Sync throw: reject finishPromise.
            ts_cancel_reject(ctx, &stream_val, &mut controller, writable, e)?;
        }
    }

    if let Some(tcp) = controller.payload() {
        if let Some(fp) = tcp.finish_promise {
            return Ok(JSValue::from(fp));
        }
    }
    Ok(JSValue::UNDEFINED)
}

/// Fulfilled branch of transformer.cancel() (source cancel path):
/// if writable is errored, reject finishPromise with its stored error;
/// otherwise error writable with the cancel reason, unblock write,
/// and resolve finishPromise.
fn ts_cancel_fulfill(
    ctx: &mut ScopedContext,
    stream_val: &JSValue,
    controller: &mut JSTransformStreamDefaultController,
    writable: JSWritableStream,
    reason: JSValue,
) -> JSResult {
    // If writable is already errored, reject finishPromise with its storedError.
    if let Some(wsp) = writable.payload()
        && wsp.state == crate::whatwg::stream::RsState::Errored
    {
        let error = wsp.stored_error;
        if let Some(tcp) = controller.payload_mut()
            && let Some(mut fp) = tcp.finish_promise
        {
            fp.reject(ctx, error);
        }
        return Ok(JSValue::UNDEFINED);
    }

    // Error the writable with the cancel reason.
    if let Some(wsp) = writable.payload()
        && let Some(mut wc) = wsp.controller
        && let Some(wcp) = wc.payload_mut()
        && let Some(mut ws) = wcp.stream
        && let Some(ws_payload) = ws.payload_mut()
        && ws_payload.state == crate::whatwg::stream::RsState::Readable
    {
        ws_payload.start_erroring(ctx, reason)?;
    }
    transform_stream_unblock_write(ctx, stream_val)?;
    if let Some(tcp) = controller.payload_mut()
        && let Some(mut fp) = tcp.finish_promise
    {
        fp.resolve(ctx, JSValue::UNDEFINED);
    }
    Ok(JSValue::UNDEFINED)
}

/// Rejected branch of transformer.cancel(): error writable with the
/// rejection reason, unblock write, reject finishPromise.
fn ts_cancel_reject(
    ctx: &mut ScopedContext,
    stream_val: &JSValue,
    controller: &mut JSTransformStreamDefaultController,
    writable: JSWritableStream,
    error: JSValue,
) -> JSResult {
    if let Some(wsp) = writable.payload()
        && let Some(mut wc) = wsp.controller
        && let Some(wcp) = wc.payload_mut()
        && let Some(mut ws) = wcp.stream
        && let Some(ws_payload) = ws.payload_mut()
        && ws_payload.state == crate::whatwg::stream::RsState::Readable
    {
        ws_payload.start_erroring(ctx, error)?;
    }
    transform_stream_unblock_write(ctx, stream_val)?;
    if let Some(tcp) = controller.payload_mut()
        && let Some(mut fp) = tcp.finish_promise
    {
        fp.reject(ctx, error);
    }
    Ok(JSValue::UNDEFINED)
}

/// Promise reaction: async transformer.cancel() fulfilled.
fn ts_source_cancel_ok(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let stream_val = cc.data(0);
    let mut controller = cc.data(1)
        .as_native_object::<JSTransformStreamDefaultController>()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;
    let reason = cc.data(2);
    let writable = if let Some(tcp) = controller.payload() {
        tcp.writable_stream
    } else {
        return Ok(JSValue::UNDEFINED);
    };
    ts_cancel_fulfill(ctx, &stream_val, &mut controller, writable, reason)
}

/// Promise reaction: async transformer.cancel() rejected.
fn ts_source_cancel_err(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let stream_val = cc.data(0);
    let mut controller = cc.data(1)
        .as_native_object::<JSTransformStreamDefaultController>()
        .ok_or_else(|| ctx.throw_internal_error("invalid controller"))?;
    let error = cc.arg(0);
    let writable = if let Some(tcp) = controller.payload() {
        tcp.writable_stream
    } else {
        return Ok(JSValue::UNDEFINED);
    };
    ts_cancel_reject(ctx, &stream_val, &mut controller, writable, error)
}

/// TransformStreamSetBackpressure (spec §6.4.1).
/// Resolve the old backpressureChangePromise, create a new one, set the flag.
/// Also mirrors the state on the TransformControllerPayload for fast access
/// from the write path.
pub(super) fn transform_stream_set_backpressure(
    ctx: &mut ScopedContext,
    stream_val: &JSValue,
    backpressure: bool,
) -> JSResult {
    let Some(tsp) = JSTransformStream::payload_of(stream_val) else {
        return Ok(JSValue::UNDEFINED);
    };

    // Resolve the OLD promise on the controller's Rc<Cell> (this is what
    // the fast path in js_writer_write is waiting on).
    let ctrl_handle = tsp.controller;
    if let Some(mut ctrl) = ctrl_handle
        && let Some(tcp) = ctrl.payload_mut()
    {
        if let Some(mut old_bp) = tcp.backpressure_change_promise.get() {
            old_bp.resolve(ctx, JSValue::UNDEFINED);
        }
    }

    // Create new promise and set it everywhere.
    let (new_promise, _rr) = Promise::create(ctx)?;

    // Set on the TransformStream payload.
    let ts_handle = match stream_val.as_native_object::<JSTransformStream>() {
        Some(h) => h,
        None => return Ok(JSValue::UNDEFINED),
    };
    let mut ts_val = JSValue::from(ts_handle);
    if let Some(tsp_mut) = JSTransformStream::payload_mut_of(&mut ts_val) {
        tsp_mut.backpressure = backpressure;
        tsp_mut.backpressure_change_promise = Some(new_promise);
    }
    // Set on the controller's Rc<Cell> (shared with the fast path).
    if let Some(mut ctrl) = ctrl_handle
        && let Some(tcp) = ctrl.payload_mut()
    {
        tcp.backpressure.set(backpressure);
        tcp.backpressure_change_promise.set(Some(new_promise));
    }
    Ok(JSValue::UNDEFINED)
}

/// TransformStreamUnblockWrite (spec §6.4.1).
/// If backpressure is true, set it to false.
pub(super) fn transform_stream_unblock_write(ctx: &mut ScopedContext, stream_val: &JSValue) -> JSResult {
    let needs_unblock = JSTransformStream::payload_of(stream_val)
        .map(|p| p.backpressure)
        .unwrap_or(false);
    if needs_unblock {
        transform_stream_set_backpressure(ctx, stream_val, false)?;
    }
    Ok(JSValue::UNDEFINED)
}

/// TransformStreamErrorWritableAndUnblockWrite (spec §6.4.2).
/// Error the writable side with `e` and relieve backpressure — the step
/// `TransformStreamDefaultControllerEnqueue` runs when the readable side's
/// enqueue (including the strategy's `size` algorithm) throws.
pub(super) fn transform_stream_error_writable_and_unblock_write(
    ctx: &mut ScopedContext,
    stream_val: &JSValue,
    e: JSValue,
) -> JSResult {
    if let Some(tsp) = JSTransformStream::payload_of(stream_val)
        && let Some(w) = tsp.writable
        && let Some(wsp) = w.payload()
        && wsp.state == crate::whatwg::stream::RsState::Readable
        && let Some(mut wc) = wsp.controller
        && let Some(wcp) = wc.payload()
        && let Some(mut ws) = wcp.stream
        && let Some(ws_payload) = ws.payload_mut()
    {
        ws_payload.start_erroring(ctx, e)?;
    }
    transform_stream_unblock_write(ctx, stream_val)?;
    Ok(JSValue::UNDEFINED)
}

fn js_transform_stream_readable(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStream"))?;
    Ok(p.readable.map_or(JSValue::UNDEFINED, JSValue::from))
}

fn js_transform_stream_writable(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let p = JSTransformStream::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a TransformStream"))?;
    Ok(p.writable.map_or(JSValue::UNDEFINED, JSValue::from))
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_transform_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTransformStream::register(rt)
}
