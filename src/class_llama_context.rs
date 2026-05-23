use std::{
    borrow::Cow,
    cell::Cell,
    ffi::c_void,
    ops::DerefMut,
    sync::{Arc, Mutex},
    time::Duration,
};

use linkme::distributed_slice;
use llama_cpp_ffi::{LlamaContext, SamplerChain};
use zuqe::{
    CallContext, ClassID, FieldDef, IntOrFloat, JS_PROP_ENUMERABLE, JS_PROP_FORCE, JSResult,
    JSValue, NativeObjectWithPayload, NativePayload, Object, Runtime, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
    def_native_object_with_payload,
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_engine::{JSEngine, parse_engine_params},
    class_llama_sampler::get_sampler_chain_arc,
    engine::{Engine, Token},
    worker::WorkerPool,
};

/// 解析 JavaScript 值中的 token 数据
/// 支持 Int32Array、普通数组(fast array)或单个整数
/// 返回 Cow 以允许调用方决定是否需要 owned 数据
pub(super) fn parse_tokens<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<Cow<'a, [i32]>, i32> {
    if let Some(ta) = val.as_object_TypedArray() {
        if let Some(lst) = ta.as_i32_array() {
            Ok(Cow::Borrowed(lst))
        } else {
            ctx.throw_type_error("expect Int32Array object");
            Err(-1)
        }
    } else if let Some(arr) = val.as_object_Array()
        && arr.is_fast_array()
    {
        // 普通数组需要转换为 owned
        let a = arr.payload();
        let mut lst: Vec<i32> = Vec::with_capacity(a.len());
        for v in a.values() {
            lst.push(v.to_number(ctx)?.to_i32());
        }
        Ok(Cow::Owned(lst))
    } else if let JSValue::Int(i) = val {
        Ok(Cow::Owned(vec![*i]))
    } else {
        ctx.throw_type_error("invalid input token");
        Err(-1)
    }
}

const PROP_CTX_SIZE: i32 = 1;
const PROP_BATCH_SIZE: i32 = 2;
const PROP_SEQUENCES: i32 = 3;
const PROP_IS_DISPOSED: i32 = 4;

#[repr(transparent)]
pub(crate) struct LlamaContextWrap {
    inner: Mutex<Option<LlamaContext>>,
}

impl LlamaContextWrap {
    fn new(llm_ctx: LlamaContext) -> Self {
        Self {
            inner: Mutex::new(Some(llm_ctx)),
        }
    }

    fn with_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut LlamaContext) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut guard = self.inner.lock().unwrap();
        let llm_ctx = guard
            .as_mut()
            .ok_or_else(|| "Context was disposed".to_string())?;
        f(llm_ctx)
    }

    fn decode_tokens(
        &self,
        tokens: Vec<i32>,
        start_pos: i32,
        logits_last: bool,
    ) -> Result<Option<i32>, String> {
        self.with_ctx_mut(|llm_ctx| {
            let seq_id = 0;
            let len = tokens.len();
            let batch = llm_ctx.batch_mut();
            let mut logits_index: Option<i32> = None;

            for (i, &t) in tokens.iter().enumerate() {
                unsafe {
                    *batch.token.add(i) = t;
                    *batch.pos.add(i) = start_pos + i as i32;
                    *(*batch.seq_id.add(i)).add(0) = seq_id;
                    *batch.n_seq_id.add(i) = 1;
                    let has_logits = logits_last && i == len - 1;
                    *batch.logits.add(i) = if has_logits { 1 } else { 0 };
                    if has_logits {
                        logits_index = Some(i as i32);
                    }
                }
            }

            batch.n_tokens = len as _;
            llm_ctx.decode()?;

            Ok(logits_index)
        })
    }

    fn sample_token(&self, logits: i32, chain: &mut SamplerChain) -> Result<i32, String> {
        self.with_ctx_mut(|llm_ctx| {
            let token = chain.sample(llm_ctx.as_ptr(), logits);
            Ok(token)
        })
    }

    fn reset(&self) -> Result<(), String> {
        self.with_ctx_mut(|llm_ctx| {
            llm_ctx.clear_memory(true);
            Ok(())
        })
    }

    fn embed_tokens(&self, tokens: Vec<i32>) -> Result<Vec<f32>, String> {
        self.with_ctx_mut(|llm_ctx| llm_ctx.embed_tokens(&tokens, 0))
    }

    fn get_stats(&self) -> Result<ContextStatsData, String> {
        self.with_ctx_mut(|llm_ctx| {
            let memory_used = llm_ctx.state_size() as i64;
            let kv_cache_size = llm_ctx.n_ctx_seq() as i64;
            let pos_max = llm_ctx.memory_seq_pos_max(0);
            let used_tokens = if pos_max >= 0 { pos_max as i64 + 1 } else { 0 };
            let max_tokens = llm_ctx.n_ctx_seq() as i64;

            Ok(ContextStatsData {
                memory_used,
                kv_cache_size,
                used_tokens,
                max_tokens,
            })
        })
    }
}

#[repr(transparent)]
#[derive(Clone)]
pub(crate) struct ContextPayload(Arc<LlamaContextWrap>);

impl NativePayload for ContextPayload {
    fn into_raw(self) -> *mut c_void {
        Box::into_raw(Box::new(self)) as *mut c_void
    }

    unsafe fn from_raw(ptr: *mut c_void) -> Self {
        let r = unsafe { Box::from_raw(ptr.cast::<Self>()) };
        *r
    }

    unsafe fn raw_as_ref<'a>(ptr: *mut c_void) -> &'a Self {
        let boxed_ptr = ptr.cast::<Self>();
        unsafe { &*boxed_ptr }
    }
}

impl From<LlamaContext> for ContextPayload {
    fn from(value: LlamaContext) -> Self {
        Self(Arc::new(LlamaContextWrap::new(value)))
    }
}

impl std::ops::Deref for ContextPayload {
    type Target = Arc<LlamaContextWrap>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Copy)]
struct ContextStatsData {
    memory_used: i64,
    kv_cache_size: i64,
    used_tokens: i64,
    max_tokens: i64,
}

fn get_llama_context(ctx: &mut ScopedContext, val: &JSValue) -> Result<ContextPayload, i32> {
    let ob = JSLlamaContext::try_from(*val).unwrap();
    ob.payload()
        .ok_or_else(|| {
            ctx.throw_type_error("Context was disposed");
            -1
        })
        .cloned()

    // let payload = val
    //     .retrive_object_payload(ctx, JSLlamaContext::class_id())?
    //     .other()
    //     .unwrap();
    // let opaque = payload.opaque();

    // if opaque.is_null() {
    //     ctx.throw_type_error("Context was disposed");
    //     Err(-1)
    // } else {
    //     Ok(unsafe { (&*opaque.cast::<Arc<LlamaContextWrap>>()).clone() })
    // }
}

fn take_llame_context(
    ctx: &mut ScopedContext,
    mut this: JSValue,
) -> Result<Option<(LlamaContext, JSValue)>, i32> {
    let p = unsafe {
        let payload = this
            .retrive_object_payload_mut(ctx, JSLlamaContext::class_id())?
            .other_mut()
            .unwrap();

        payload.replace_opaque(std::ptr::null_mut())
    };

    if !p.is_null() {
        let model_val = this.get_property_str(ctx, "model")?;
        this.delete_property_str(ctx, "model", JS_PROP_FORCE)?; // force delete

        let llm_ctx = unsafe {
            let a = Box::from_raw(p.cast::<Arc<LlamaContextWrap>>());
            a.inner.lock().unwrap().take().unwrap()
        };
        Ok(Some((llm_ctx, model_val)))
    } else {
        Ok(None)
    }
}

fn js_context_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    if cc.magic() == PROP_IS_DISPOSED {
        let p = this
            .retrive_object_payload_mut(ctx, JSLlamaContext::class_id())?
            .other_mut()
            .unwrap()
            .opaque();
        return Ok(JSValue::Bool(p.is_null()));
    }

    let llm_ctx_wrap = get_llama_context(ctx, &mut this)?;

    let val: JSValue = match cc.magic() {
        PROP_CTX_SIZE => llm_ctx_wrap
            .with_ctx_mut(|llm_ctx| Ok(llm_ctx.n_ctx()))
            .map_err(|e| {
                ctx.throw_type_error(&e);
                -1
            })?
            .into(),
        PROP_BATCH_SIZE => llm_ctx_wrap
            .with_ctx_mut(|llm_ctx| Ok(llm_ctx.n_ubatch()))
            .map_err(|e| {
                ctx.throw_type_error(&e);
                -1
            })?
            .into(),
        PROP_SEQUENCES => llm_ctx_wrap
            .with_ctx_mut(|llm_ctx| Ok(llm_ctx.n_seq_max()))
            .map_err(|e| {
                ctx.throw_type_error(&e);
                -1
            })?
            .into(),

        _ => JSValue::UNDEFINED,
    };

    Ok(val)
}

fn js_context_decode(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx_wrap = get_llama_context(ctx, &mut this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?.into_owned();

    if cc.length() < 2 {
        ctx.throw_type_error("startPosition is required");
        return Err(-1);
    }

    let n_past: i32;
    let mut logits_last = true;

    let start_pos = cc.arg(1).to_number(ctx)?;
    if let IntOrFloat::Int64(n) = start_pos
        && (0..=i32::MAX as i64).contains(&n)
    {
        n_past = n as i32;
    } else {
        ctx.throw_type_error("invalid startPosition");
        return Err(-1);
    }

    if cc.length() > 2 {
        let arg2 = cc.arg(2);
        if !matches!(arg2, JSValue::UNDEFINED) {
            logits_last = arg2.to_bool()?;
        }
    }

    let (promise, tx) =
        Promise::create_with_channel::<Result<Option<i32>, String>>(ctx, |cx, data| -> JSResult {
            match data {
                Ok(Some(idx)) => Ok(idx.into()),
                Ok(None) => Ok(JSValue::NULL),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;

    // post a decoding job to background worker
    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::singleton().add_job(
        move || {
            let res = llm_ctx_wrap.decode_tokens(tokens, n_past, logits_last);
            let _ = tx_submit.send(res);
        },
        Some(move || {
            let _ = tx_timeout.send(Err("AcquireTimeout".to_string()));
        }),
    ) {
        let _ = tx.send(Err(e.to_string()));
    }

    Ok(promise.into())
}

fn js_context_sample(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx_wrap = get_llama_context(ctx, &mut this)?;

    let logits = cc.arg(0).to_number(ctx)?.to_i32();
    let s = cc.arg(1);
    let sampler = get_sampler_chain_arc(ctx, &s)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<Token, String>>(ctx, |cx, data| -> JSResult {
            match data {
                Ok(token) => Ok(token.into()),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;

    // post a sampling job to background worker
    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::singleton().add_job(
        move || {
            let res = llm_ctx_wrap.sample_token(logits, sampler.0.lock().unwrap().deref_mut());
            let _ = tx_submit.send(res);
        },
        Some(move || {
            let _ = tx_timeout.send(Err("AcquireTimeout".to_string()));
        }),
    ) {
        let _ = tx.send(Err(e.to_string()));
    }

    Ok(promise.into())
}

fn js_context_reset(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx_wrap = get_llama_context(ctx, &mut this)?;

    let (promise, tx) =
        Promise::create_with_channel(ctx, |cx, payload: Result<(), String>| -> JSResult {
            match payload {
                Ok(()) => Ok(JSValue::UNDEFINED),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;

    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::singleton().add_job(
        move || {
            let res = llm_ctx_wrap.reset();
            let _ = tx_submit.send(res);
        },
        Some(move || {
            let _ = tx_timeout.send(Err("AcquireTimeout".to_string()));
        }),
    ) {
        let _ = tx.send(Err(e.to_string()));
    }
    Ok(promise.into())
}

fn js_context_getStats(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx_wrap = get_llama_context(ctx, &mut this)?;

    let (promise, tx) = Promise::create_with_channel(
        ctx,
        |cx, payload: Result<ContextStatsData, String>| -> JSResult {
            let stats = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            let mut obj = cx.create_base_object()?;
            obj.set_property_str(cx, "memoryUsed", stats.memory_used.into())?;
            obj.set_property_str(cx, "kvCacheSize", stats.kv_cache_size.into())?;
            obj.set_property_str(cx, "usedTokens", stats.used_tokens.into())?;
            obj.set_property_str(cx, "maxTokens", stats.max_tokens.into())?;
            Ok(obj.into())
        },
    )?;

    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::singleton().add_job(
        move || {
            let res = llm_ctx_wrap.get_stats();
            let _ = tx_submit.send(res);
        },
        Some(move || {
            let _ = tx_timeout.send(Err("AcquireTimeout".to_string()));
        }),
    ) {
        let _ = tx.send(Err(e.to_string()));
    }
    Ok(promise.into())
}

fn js_context_embedTokens(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx_wrap = get_llama_context(ctx, &mut this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?.into_owned();

    if tokens.is_empty() {
        ctx.throw_type_error("tokens cannot be empty");
        return Err(-1);
    }

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<f32>, String>>(ctx, |cx, payload| -> JSResult {
            match payload {
                Ok(v) => TypedArray::create_from_vec_f32(cx, v).map(JSValue::from),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;
    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::singleton().add_job(
        move || {
            let _ = tx_submit.send(llm_ctx_wrap.embed_tokens(tokens));
        },
        Some(move || {
            let _ = tx_timeout.send(Err("AcquireTimeout".to_string()));
        }),
    ) {
        let _ = tx.send(Err(e.to_string()));
    }
    Ok(promise.into())
}

fn js_context_createEngine(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // Parse engine options first, before taking the LlamaContext.
    let opts = if cc.length() > 0 {
        let v = cc.arg(0);
        if v.is_undefined() || v.is_null() {
            None
        } else {
            Some(v.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("engineOptions must be object");
                -1
            })?)
        }
    } else {
        None
    };
    let eng_params = parse_engine_params(ctx, opts.as_ref())?;

    // take LlamaContext out of the this object
    if let Some((llm_ctx, model_val)) = take_llame_context(ctx, cc.this())? {
        if llm_ctx.n_seq_max() == 1 {
            log::warn!("createEngine: context sequences is 1, only one parallel slot available");
        }

        let engine = Engine::start(
            llm_ctx,
            Duration::from_millis(eng_params.idle_sleep_ms),
            eng_params.slot_match_threshold,
            eng_params.max_tokens,
        );
        let mut engine_obj = JSEngine::new_instance(ctx, JSValue::UNDEFINED, engine.into())?;
        let _ = engine_obj.define_property_value_str(ctx, "model", model_val, JS_PROP_ENUMERABLE);

        Ok(engine_obj.into())
    } else {
        ctx.throw_type_error("Context was disposed");
        Err(-1)
    }
}

fn js_context_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    take_llame_context(ctx, cc.this())?;
    Ok(JSValue::UNDEFINED)
}

const CLASS_CONTEXT_PROTO: &[FieldDef] = &[
    FieldDef::GETSET(
        "contextSize",
        Some(js_context_getter),
        None,
        PROP_CTX_SIZE as _,
    ),
    FieldDef::GETSET(
        "batchSize",
        Some(js_context_getter),
        None,
        PROP_BATCH_SIZE as _,
    ),
    FieldDef::GETSET(
        "isDisposed",
        Some(js_context_getter),
        None,
        PROP_IS_DISPOSED as _,
    ),
    FieldDef::FUNC("decode", 2, js_context_decode, 0),
    FieldDef::FUNC("sample", 1, js_context_sample, 0),
    FieldDef::FUNC("reset", 0, js_context_reset, 0),
    FieldDef::FUNC("embedTokens", 1, js_context_embedTokens, 0),
    FieldDef::FUNC("getStats", 0, js_context_getStats, 0),
    FieldDef::FUNC("createEngine", 1, js_context_createEngine, 0),
    FieldDef::FUNC("dispose", 0, js_context_dispose, 0),
];

def_native_object_with_payload!(
    "Context",
    JSLlamaContext,
    ContextPayload,
    CLASS_CONTEXT_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_context(rt: &mut Runtime) -> Result<ClassID, i32> {
    rt.register_native_class_with_payload::<_, JSLlamaContext>()
}
