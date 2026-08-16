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

    // Per the WHATWG spec the second/third arguments are the writable and
    // readable queuing strategies respectively.
    let writable_strategy = cc.arg(1);
    let readable_strategy = cc.arg(2);
    // Spec §6.2.4: default writable HWM = 1, readable HWM = 0.
    let writable_hwm = extract_high_water_mark(ctx, writable_strategy)?;
    let readable_hwm = extract_high_water_mark(ctx, readable_strategy)?;

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
    {
        let (new_promise, _rr) = Promise::create(ctx)?;
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
        // Copy start/close/abort from the original transformer.
        if let Some(tobj) = transformer.as_object() {
            for method in ["start", "close", "abort"] {
                if let Ok(v) = tobj.get_property_str(ctx, method) {
                    if !v.is_undefined() {
                        wrapper.define_property_value_str(ctx, method, v, zuqe::JS_PROP_C_W_E)?;
                    }
                }
            }
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
        rcp.set_algorithms(pull_fn.into(), cancel_fn.into(), JSValue::UNDEFINED);
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

    if !has_transform {
        let enqueue_fn =
            JSValue::from(tcp.readable_controller).get_property_str(ctx, "enqueue")?;
        enqueue_fn.call(ctx, JSValue::from(tcp.readable_controller), &[chunk])?;
        return Ok(JSValue::UNDEFINED);
    }

    // Check backpressure via the TransformStream.
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
                zuqe::Callback::Func(|ctx, cc| {
                    let transformer_val = cc.data(0);
                    let tc_val = cc.data(1);
                    let chunk = cc.data(2);
                    if let Some(obj) = transformer_val.as_object() {
                        let transform_fn = obj.get_property_str(ctx, "transform")?;
                        transform_fn.call(ctx, transformer_val, &[chunk, tc_val])?;
                    }
                    Ok(JSValue::UNDEFINED)
                }),
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
            bp.then(ctx, on_ok.into(), on_err.into())?;
            return Ok(JSValue::from(bp));
        }
    }

    let transform_fn = transformer_obj.get_property_str(ctx, "transform")?;
    let tc_val = JSValue::from(tc_handle);
    let result = transform_fn.call(ctx, tcp.transformer, &[chunk, tc_val])?;
    Ok(result)
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
        Ok(_v) => {
            // Fulfilled: error the writable with reason, unblock write,
            // resolve finishPromise.
            if let Some(wsp) = writable.payload()
                && let Some(mut wc) = wsp.controller
                && let Some(wcp) = wc.payload_mut()
                && let Some(mut ws) = wcp.stream
                && let Some(ws_payload) = ws.payload_mut()
            {
                if ws_payload.state == crate::whatwg::stream::RsState::Readable {
                    ws_payload.start_erroring(ctx, reason)?;
                }
            }
            // TransformStreamUnblockWrite
            transform_stream_unblock_write(ctx, &stream_val)?;
            // Resolve finishPromise
            if let Some(tcp) = controller.payload_mut()
                && let Some(mut fp) = tcp.finish_promise
            {
                fp.resolve(ctx, JSValue::UNDEFINED);
            }
        }
        Err(_) => {
            let e = ctx.take_exception().to_value(ctx);
            // Rejected: error the writable with the thrown error, unblock write,
            // reject finishPromise.
            if let Some(wsp) = writable.payload()
                && let Some(mut wc) = wsp.controller
                && let Some(wcp) = wc.payload_mut()
                && let Some(mut ws) = wcp.stream
                && let Some(ws_payload) = ws.payload_mut()
            {
                if ws_payload.state == crate::whatwg::stream::RsState::Readable {
                    ws_payload.start_erroring(ctx, e)?;
                }
            }
            transform_stream_unblock_write(ctx, &stream_val)?;
            if let Some(tcp) = controller.payload_mut()
                && let Some(mut fp) = tcp.finish_promise
            {
                fp.reject(ctx, e);
            }
        }
    }

    if let Some(tcp) = controller.payload() {
        if let Some(fp) = tcp.finish_promise {
            return Ok(JSValue::from(fp));
        }
    }
    Ok(JSValue::UNDEFINED)
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
fn transform_stream_unblock_write(ctx: &mut ScopedContext, stream_val: &JSValue) -> JSResult {
    let needs_unblock = JSTransformStream::payload_of(stream_val)
        .map(|p| p.backpressure)
        .unwrap_or(false);
    if needs_unblock {
        transform_stream_set_backpressure(ctx, stream_val, false)?;
    }
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
