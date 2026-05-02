use std::{cell::Cell, ffi::c_void, sync::Arc};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_context::parse_tokens,
    engine::{Engine, EngineReply, OutputMode, TaskConfig},
};

pub(super) struct EngineParams {
    pub idle_sleep_ms: u64,
    pub slot_match_threshold: f32,
    /// Default max tokens for generation; 0 means unlimited.
    pub max_tokens: usize,
}

pub(super) fn parse_engine_params(
    ctx: &mut ScopedContext,
    opts: Option<&Object>,
) -> Result<EngineParams, i32> {
    let mut idle_sleep_ms = 1u64;
    let mut slot_match_threshold = 0.5f32;
    let mut max_tokens = 0usize;
    if let Some(opts) = opts {
        let v = opts.get_property_str(ctx, "idleSleepMs")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                idle_sleep_ms = n as u64;
            } else {
                ctx.throw_type_error("idleSleepMs must be >= 0");
                return Err(-1);
            }
        }

        let v = opts.get_property_str(ctx, "slotMatchThreshold")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..=1.0).contains(&d) {
                slot_match_threshold = d as f32;
            } else {
                ctx.throw_type_error("slotMatchThreshold must be in [0, 1]");
                return Err(-1);
            }
        }

        let v = opts.get_property_str(ctx, "maxTokens")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n < 0 {
                ctx.throw_type_error("maxTokens must be >= 0");
                return Err(-1);
            }
            max_tokens = n as usize;
        }
    }
    Ok(EngineParams {
        idle_sleep_ms,
        slot_match_threshold,
        max_tokens,
    })
}

fn get_engine<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<&'a Engine, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaEngine::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("Engine was disposed");
        Err(-1)
    } else {
        Ok(unsafe { &*opaque.cast::<Engine>() })
    }
}

pub(super) fn get_engine_arc(ctx: &mut ScopedContext, val: &JSValue) -> Result<Arc<Engine>, i32> {
    // opaque was created from Arc::into_raw(), so it points to the Engine
    // inside the Arc allocation. Increment the strong count so the returned
    // Arc can outlive the JS object.
    let engine_ref = get_engine(ctx, val)?;
    let ptr = engine_ref as *const Engine as *mut Engine;
    unsafe { Arc::increment_strong_count(ptr) };
    Ok(unsafe { Arc::from_raw(ptr) })
}

const MAGIC_CHAT: i32 = 0;
const MAGIC_CHAT_STREAM: i32 = 1;

fn js_engine_chat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?.into_owned();
    if tokens.is_empty() {
        ctx.throw_type_error("tokens cannot be empty");
        return Err(-1);
    }

    let mode = if cc.magic() == MAGIC_CHAT {
        OutputMode::OneShot
    } else {
        OutputMode::Continuous
    };

    let rx = engine
        .push_task(tokens, TaskConfig::new().output_mode(mode))
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    if cc.magic() == MAGIC_CHAT {
        // generate()
        let promise =
            Promise::create_with_receiver::<EngineReply>(ctx, rx, |cx, msg| -> JSResult {
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
                    EngineReply::Stop(_, _) => {
                        cx.throw_internal_error("unexpected message in oneshot mode");
                        Err(-1)
                    }
                }
            })?;
        Ok(promise.into())
    } else {
        // generateStream()
        AsyncIterableIterator::create_with_receiver(
            ctx,
            rx,
            |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    EngineReply::Text(text) => cx
                        .create_string_from_utf8(text.into_bytes())
                        .map(JSValue::from)
                        .map(Some),
                    EngineReply::Stop(_, _) => Ok(None),
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
}

fn js_engine_httpServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine_arc(ctx, &this)?;

    // Parse options object
    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8080;

    let opts = if cc.length() > 0 {
        let v = cc.arg(0);
        if v.is_undefined() || v.is_null() {
            None
        } else {
            Some(v.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("options must be object");
                -1
            })?)
        }
    } else {
        None
    };
    if let Some(opts) = opts.as_ref() {
        let v = opts.get_property_str(ctx, "host")?;
        if !v.is_undefined() {
            let s = v.to_string(ctx)?;
            host = s.content().encode_utf8(false).to_string();
        }

        let v = opts.get_property_str(ctx, "port")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 || n > 65535 {
                ctx.throw_type_error("port must be 1..65535");
                return Err(-1);
            }
            port = n as u16;
        }
    }

    if engine.is_serving() {
        ctx.throw_type_error("engine is already serving");
        return Err(-1);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    engine.set_serving(shutdown_tx);

    let result_slot = PromiseResultSlot::<Result<(), String>>::new();

    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, res| match res {
            Ok(()) => Ok(JSValue::UNDEFINED),
            Err(e) => {
                cx.throw_internal_error(&e);
                Err(-1)
            }
        })?;

    tokio::spawn(async move {
        let res = engine.clone().http_serve(&host, port, shutdown_rx).await;

        engine.unset_serving();
        let _ = result_slot.commit(res);
    });

    Ok(promise.into())
}

fn js_engine_stopServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    engine.request_stop();
    Ok(JSValue::UNDEFINED)
}

fn js_engine_isServing(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    Ok(JSValue::Bool(engine.is_serving()))
}

fn js_engine_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaEngine::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Arc::from_raw(p.cast::<Engine>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

const CLASS_ENGINE_PROTO: &[FieldDef] = &[
    FieldDef::CALL("chat", 1, js_engine_chat, MAGIC_CHAT as i16),
    FieldDef::CALL("chatStream", 1, js_engine_chat, MAGIC_CHAT_STREAM as i16),
    FieldDef::CALL("httpServe", 1, js_engine_httpServe, 0),
    FieldDef::CALL("stopServe", 0, js_engine_stopServe, 0),
    FieldDef::GETSET("isServing", Some(js_engine_isServing), None, 0),
    FieldDef::CALL("dispose", 0, js_engine_dispose, 0),
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
                        drop(unsafe { Arc::from_raw(opaque.cast::<Engine>()) });
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
        engine: Arc<Engine>,
    ) -> Result<Object, i32> {
        let opaque = Arc::into_raw(engine) as *mut c_void;

        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Arc::from_raw(p.opaque().cast::<Engine>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_engine(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaEngine::register(rt)
}
