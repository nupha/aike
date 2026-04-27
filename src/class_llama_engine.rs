use std::{
    cell::Cell,
    ffi::c_void,
    sync::atomic::{AtomicU64, Ordering},
};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise},
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_context::parse_tokens,
    engine::{EngineReply, InferenceEngine, OutputMode},
};

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

fn get_engine<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<&'a InferenceEngine, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaEngine::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("Engine was disposed");
        Err(-1)
    } else {
        Ok(unsafe { &*opaque.cast::<InferenceEngine>() })
    }
}

fn completion(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?.into_owned();
    if tokens.is_empty() {
        ctx.throw_type_error("tokens cannot be empty");
        return Err(-1);
    }

    let task_id = format!("{}", NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed));

    let rx = engine
        .push_task(task_id, tokens, Vec::new(), OutputMode::OneShot)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    let promise = Promise::create_with_receiver(ctx, rx, |cx, msg| -> JSResult {
        match msg {
            EngineReply::Text(text) => cx
                .create_string_from_utf8(text.into_bytes())
                .map(JSValue::from),
            EngineReply::Error(e) => {
                cx.throw_internal_error(&e);
                Err(-1)
            }
            EngineReply::Cancelled => {
                cx.throw_internal_error("task cancelled");
                Err(-1)
            }
            EngineReply::Shutdown => {
                cx.throw_internal_error("engine shutdown");
                Err(-1)
            }
            EngineReply::Stop(_) => {
                cx.throw_internal_error("unexpected message in oneshot mode");
                Err(-1)
            }
        }
    })?;

    Ok(promise.into())
}

fn completion_iter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?.into_owned();
    if tokens.is_empty() {
        ctx.throw_type_error("tokens cannot be empty");
        return Err(-1);
    }

    let task_id = format!("{}", NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed));

    let rx = engine
        .push_task(task_id, tokens, Vec::new(), OutputMode::Continuous)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    AsyncIterableIterator::create_with_receiver(
        ctx,
        rx,
        |cx, msg| -> Result<Option<JSValue>, i32> {
            match msg {
                EngineReply::Text(text) => cx
                    .create_string_from_utf8(text.into_bytes())
                    .map(JSValue::from)
                    .map(Some),
                EngineReply::Stop(_) => Ok(None),
                EngineReply::Error(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
                EngineReply::Cancelled => {
                    cx.throw_internal_error("task cancelled");
                    Err(-1)
                }
                EngineReply::Shutdown => {
                    cx.throw_internal_error("engine shutdown");
                    Err(-1)
                }
            }
        },
    )
    .map(JSValue::from)
}

fn dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaEngine::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Box::from_raw(p.cast::<InferenceEngine>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

const CLASS_ENGINE_PROTO: &[FieldDef] = &[
    FieldDef::CALL("completion", 1, completion, 0),
    FieldDef::CALL("completionIter", 1, completion_iter, 0),
    FieldDef::CALL("dispose", 0, dispose, 0),
];

pub struct JSLlamaEngine;

impl JSLlamaEngine {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("Engine", CLASS_ENGINE_PROTO, None).on_dispose(
                |_, opaque: *mut c_void, _| {
                    if !opaque.is_null() {
                        drop(unsafe { Box::from_raw(opaque.cast::<InferenceEngine>()) });
                    }
                },
            ),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub(super) fn new_instance(
        ctx: &mut ScopedContext,
        engine: InferenceEngine,
    ) -> Result<Object, i32> {
        let opaque = Box::into_raw(Box::new(engine)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<InferenceEngine>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_engine(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaEngine::register(rt)
}
