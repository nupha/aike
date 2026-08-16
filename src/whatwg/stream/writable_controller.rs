// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WritableStreamDefaultController: the controller handed to an underlying
//! sink's `start()`/`write()` callbacks.

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    USER_CLASS_DEFS,
    whatwg::{
        abort::make_default_signal,
        stream::{
            transform_controller::JSTransformStreamDefaultController, writable::JSWritableStream,
        },
    },
};
pub(crate) struct WritableControllerPayload {
    pub(crate) stream: Option<JSWritableStream>,
    pub(crate) underlying_sink: JSValue,
    pub(crate) transform_controller: Option<JSTransformStreamDefaultController>,
    pub(crate) high_water_mark: f64,
    /// The `AbortSignal` exposed as `controller.signal`. Aborted synchronously
    /// when the stream is aborted (per the Streams spec). Created lazily via
    /// `ensure_signal`.
    pub(crate) signal: Option<JSValue>,
    /// The queuing strategy's `size` algorithm (a JS function), or `undefined`
    /// when no strategy was supplied (default chunk size is 1.0).
    pub(crate) size_algorithm: JSValue,
}

impl WritableControllerPayload {
    pub(crate) fn new(
        underlying_sink: JSValue,
        stream: Option<JSWritableStream>,
        high_water_mark: f64,
        size_algorithm: JSValue,
    ) -> Self {
        Self {
            stream,
            underlying_sink,
            transform_controller: None,
            high_water_mark,
            signal: None,
            size_algorithm,
        }
    }

    /// Lazily create (once) and return this controller's `AbortSignal`.
    pub(crate) fn ensure_signal(&mut self, ctx: &mut ScopedContext) -> Result<JSValue, i32> {
        if self.signal.is_none() {
            let s = make_default_signal(ctx)?;
            self.signal = Some(s);
        }
        Ok(self.signal.unwrap())
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_sink.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref tc) = self.transform_controller {
            tc.trace(gcx);
        }
        if let Some(ref sig) = self.signal {
            sig.trace(gcx);
        }
        self.size_algorithm.trace(gcx);
    }
}

const WRITABLE_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("error", 1, js_writable_controller_error, 0),
    FieldDef::GETSET("signal", Some(js_writable_controller_signal), None, 0),
    FieldDef::STR(
        "[Symbol.toStringTag]",
        "WritableStreamDefaultController",
        zuqe::JS_PROP_CONFIGURABLE,
    ),
];

fn js_writable_controller_constructor(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Err(ctx.throw_type_error("WritableStreamDefaultController cannot be constructed directly"))
}

def_native_object!(
    "WritableStreamDefaultController",
    JSWritableStreamDefaultController,
    WritableControllerPayload,
    WRITABLE_CONTROLLER_PROTO,
    Some(ClassCtorDef::new(js_writable_controller_constructor, 0, false).expose_to_global()),
    on_payload_gc: WritableControllerPayload::gc_trace
);

fn js_writable_controller_error(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let e = cc.arg(0);
    let this = cc.this();

    // Per spec `WritableStreamDefaultControllerError`: error the stream,
    // rejecting the writer's `ready`/`closed`, queued writes, and a pending
    // close request. An in-flight write keeps running and settles on its own.
    if let Some(p) = JSWritableStreamDefaultController::payload_of(&this)
        && let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        sp.start_erroring(_ctx, e)?;
    }

    Ok(JSValue::UNDEFINED)
}

fn js_writable_controller_signal(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let mut p = JSWritableStreamDefaultController::payload_mut_of(&mut this)
        .ok_or_else(|| ctx.throw_type_error("not a WritableStreamDefaultController"))?;
    p.ensure_signal(ctx)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStreamDefaultController::register(rt)
}
