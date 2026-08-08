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
    whatwg::stream::{
        transform_controller::JSTransformStreamDefaultController, writable::JSWritableStream,
    },
};

use super::RsState;

pub(crate) struct WritableControllerPayload {
    pub(crate) stream: Option<JSWritableStream>,
    pub(crate) underlying_sink: JSValue,
    pub(crate) transform_controller: Option<JSTransformStreamDefaultController>,
    pub(crate) high_water_mark: f64,
}

impl WritableControllerPayload {
    pub(crate) fn new(
        underlying_sink: JSValue,
        stream: Option<JSWritableStream>,
        high_water_mark: f64,
    ) -> Self {
        Self {
            stream,
            underlying_sink,
            transform_controller: None,
            high_water_mark,
        }
    }

    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        self.underlying_sink.trace(gcx);
        if let Some(ref s) = self.stream {
            s.trace(gcx);
        }
        if let Some(ref tc) = self.transform_controller {
            tc.trace(gcx);
        }
    }
}

const WRITABLE_CONTROLLER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("error", 1, js_writable_controller_error, 0),
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

    if let Some(p) = JSWritableStreamDefaultController::payload_of(&this)
        && let Some(mut stream_val) = p.stream
        && let Some(sp) = stream_val.payload_mut()
    {
        // Spec: error() is a no-op once the stream has left the writable state.
        if sp.state != RsState::Readable {
            return Ok(JSValue::UNDEFINED);
        }
        sp.state = RsState::Errored;
        sp.stored_error = e;
    }

    Ok(JSValue::UNDEFINED)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_writable_stream_default_controller(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWritableStreamDefaultController::register(rt)
}
