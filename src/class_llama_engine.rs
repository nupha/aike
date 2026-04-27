use std::{
    cell::Cell,
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use linkme::distributed_slice;
use llama_cpp_ffi::LlamaContext;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue, Object,
    Runtime, ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise},
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_context::parse_tokens,
    class_llama_model::{get_llama_model, parse_context_params},
    engine::{Engine, EngineReply, OutputMode},
};

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct EngineParams {
    pub max_parallel: usize,
    pub idle_sleep_ms: u64,
    pub slot_match_threshold: f32,
}

pub(super) fn parse_engine_params(
    ctx: &mut ScopedContext,
    opts: Option<&Object>,
) -> Result<EngineParams, i32> {
    let mut max_parallel = 1usize;
    let mut idle_sleep_ms = 1u64;
    let mut slot_match_threshold = 0.5f32;
    if let Some(opts) = opts {
        let v = opts.get_property_str(ctx, "maxParallel")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 1 {
                max_parallel = n as usize;
            } else {
                ctx.throw_type_error("maxParallel must be >= 1");
                return Err(-1);
            }
        }

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
    }
    Ok(EngineParams {
        max_parallel,
        idle_sleep_ms,
        slot_match_threshold,
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
            drop(Arc::from_raw(p.cast::<Engine>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

/// Create an Engine from JS arguments (contextOptions object, engineOptions object).
/// Parses both options, creates the underlying LlamaContext, and starts the Engine.
pub(super) fn create_engine_from_js(
    ctx: &mut ScopedContext,
    model: llama_cpp_ffi::LlamaModel,
    ctx_opts: &Object,
    eng_opts: &Object,
) -> Result<Engine, i32> {
    let ctx_params = parse_context_params(ctx, Some(ctx_opts))?;
    let eng_params = parse_engine_params(ctx, Some(eng_opts))?;

    let seq_cap = ctx_params.n_seq_max.max(1) as usize;
    let mut max_parallel = eng_params.max_parallel;
    if max_parallel > seq_cap {
        log::warn!(
            "createEngine: maxParallel ({}) is greater than sequences ({}); clamping maxParallel to sequences",
            max_parallel,
            seq_cap,
        );
        max_parallel = seq_cap;
    }

    let mut llm_ctx = LlamaContext::new(model, ctx_params).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    llm_ctx.init_batch(ctx_params.n_batch as i32, 0, ctx_params.n_seq_max as i32);

    Ok(Engine::start(
        max_parallel,
        Duration::from_millis(eng_params.idle_sleep_ms),
        eng_params.slot_match_threshold,
        llm_ctx,
    ))
}

pub(super) fn js_model_createEngine(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    if cc.length() < 2 {
        ctx.throw_type_error("requires contextOptions and engineOptions arguments");
        return Err(-1);
    }

    // arg0: context options (required, must be object)
    let v0 = cc.arg(0);
    let ctx_opts = v0.as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("contextOptions must be object");
        -1
    })?;

    // arg1: engine options (required, must be object)
    let v1 = cc.arg(1);
    let eng_opts = v1.as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("engineOptions must be object");
        -1
    })?;

    let engine = create_engine_from_js(ctx, model, &ctx_opts, &eng_opts)?;

    let mut obj = JSLlamaEngine::new_instance(ctx, engine)?;
    // Keep model alive as long as pipeline exists.
    obj.define_property_value_str(ctx, "model", this, JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

const CLASS_ENGINE_PROTO: &[FieldDef] = &[
    FieldDef::CALL("completion", 1, completion, 0),
    FieldDef::CALL("completionIter", 1, completion_iter, 0),
    FieldDef::CALL(
        "httpServe",
        1,
        crate::class_openai_server::engine_httpServe,
        0,
    ),
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

    pub(super) fn new_instance(ctx: &mut ScopedContext, engine: Engine) -> Result<Object, i32> {
        let eng = Arc::new(engine);
        let opaque = Arc::into_raw(eng) as *mut c_void;

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
