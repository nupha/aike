use std::{
    borrow::Cow,
    cell::Cell,
    ffi::c_void,
    sync::{Arc, Mutex},
};

use linkme::distributed_slice;
use llama_cpp_ffi::{LlamaContext, SamplingParams};
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, Object, Runtime,
    RuntimeImpl, ScopedContext, builtins::promise::Promise,
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_sampler::{SamplerId, get_sampler_id, get_sampler_spec_by_id},
    worker::WorkerPool,
};

const PROP_CTX_SIZE: i32 = 1;
const PROP_BATCH_SIZE: i32 = 2;
const PROP_SEQUENCES: i32 = 3;
const PROP_IS_DISPOSED: i32 = 4;

struct LlamaContextState {
    inner: Mutex<Option<LlamaContext<'static>>>,
}

#[derive(Clone, Copy)]
struct ContextStatsData {
    memory_used: i64,
    kv_cache_size: i64,
    used_tokens: i64,
    max_tokens: i64,
}

impl LlamaContextState {
    fn new(llm_ctx: LlamaContext<'static>) -> Self {
        Self {
            inner: Mutex::new(Some(llm_ctx)),
        }
    }

    fn is_disposed(&self) -> bool {
        self.inner.lock().unwrap().is_none()
    }

    fn with_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut LlamaContext<'static>) -> Result<R, String>,
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

    fn sample_token(
        &self,
        sampler_id: SamplerId,
        logits: i32,
        sampler_spec: crate::class_llama_sampler::SamplerSpec,
        params_opt: Option<SamplingParams>,
    ) -> Result<i32, String> {
        self.with_ctx_mut(|llm_ctx| {
            let token = crate::worker::sample_with_cached_sampler(
                llm_ctx,
                sampler_id,
                sampler_spec.rev,
                sampler_spec.vocab_ptr,
                sampler_spec.ctx_train,
                &sampler_spec.steps,
                logits,
                params_opt.as_ref(),
            )?;
            Ok(token)
        })
    }

    fn reset(&self) -> Result<(), String> {
        self.with_ctx_mut(|llm_ctx| {
            llm_ctx.clear_memory(true);
            Ok(())
        })
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

    fn dispose(&self) {
        let mut guard = self.inner.lock().unwrap();
        guard.take();
    }
}

fn get_llama_context_state(
    ctx: &mut ScopedContext,
    val: &mut JSValue,
) -> Result<Arc<LlamaContextState>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaContext::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();

    if opaque.is_null() {
        ctx.throw_type_error("Context was disposed");
        Err(-1)
    } else {
        Ok(unsafe { (&*opaque.cast::<Arc<LlamaContextState>>()).clone() })
    }
}

fn get_prop(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    if cc.magic() == PROP_IS_DISPOSED {
        let state = get_llama_context_state(ctx, &mut this)?;
        let disposed = state.is_disposed();
        return Ok(JSValue::Bool(disposed));
    }

    let state = get_llama_context_state(ctx, &mut this)?;

    let val: JSValue = match cc.magic() {
        PROP_CTX_SIZE => state
            .with_ctx_mut(|llm_ctx| Ok(llm_ctx.n_ctx()))
            .map_err(|e| {
                ctx.throw_type_error(&e);
                -1
            })?
            .into(),
        PROP_BATCH_SIZE => state
            .with_ctx_mut(|llm_ctx| Ok(llm_ctx.n_ubatch()))
            .map_err(|e| {
                ctx.throw_type_error(&e);
                -1
            })?
            .into(),
        PROP_SEQUENCES => state
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

fn decode(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let state = get_llama_context_state(ctx, &mut this)?;

    let arg0 = cc.arg(0);
    let data: Cow<'_, [i32]> = if let Some(ta) = arg0.as_object_TypedArray() {
        if let Some(lst) = ta.as_i32_array() {
            Cow::Borrowed(lst)
        } else {
            ctx.throw_type_error("expect Int32Array object");
            return Err(-1);
        }
    } else if let Some(arr) = arg0.as_object_Array()
        && arr.is_fast_array()
    {
        let a = arr.payload();
        let len = a.len();
        let mut lst = Vec::<i32>::with_capacity(len);
        for v in a.values() {
            let v = v.to_number(ctx)?.to_i32();
            lst.push(v);
        }
        Cow::Owned(lst)
    } else if let JSValue::Int(tt) = arg0 {
        Cow::Borrowed(&[tt])
    } else {
        ctx.throw_type_error("invalid input token");
        return Err(-1);
    };

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

    let tokens = data.into_owned();
    let (promise, tx) = Promise::create_from_sender(
        ctx,
        |cx, payload: Result<Option<i32>, String>| -> JSResult {
            match payload {
                Ok(Some(idx)) => Ok(idx.into()),
                Ok(None) => Ok(JSValue::NULL),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        },
    )?;

    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::global().submit(
        move || {
            let res = state.decode_tokens(tokens, n_past, logits_last);
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

fn sample(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let state = get_llama_context_state(ctx, &mut this)?;

    let logits = cc.arg(0).to_number(ctx)?.to_i32();
    let s = cc.arg(1);
    let sampler_id: SamplerId = get_sampler_id(ctx, &s)?;

    let mut params_opt: Option<SamplingParams> = None;
    if cc.length() > 2 {
        let p = cc.arg(2);
        if !p.is_undefined() {
            let obj = p.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("params must be object");
                -1
            })?;
            let mut params = SamplingParams::default();

            let v = obj.get_property_str(ctx, "temperature")?;
            if !v.is_undefined() {
                let t = v.to_number(ctx)?.to_f64() as f32;
                params.temperature = Some(t);
            }

            let v = obj.get_property_str(ctx, "topP")?;
            if !v.is_undefined() {
                let p = v.to_number(ctx)?.to_f64() as f32;
                params.top_p = Some(p);
            }

            let v = obj.get_property_str(ctx, "topK")?;
            if !v.is_undefined() {
                let k = v.to_number(ctx)?.to_i32();
                params.top_k = Some(k);
            }

            let v = obj.get_property_str(ctx, "repeatPenalty")?;
            if !v.is_undefined() {
                let rp = v.to_number(ctx)?.to_f64() as f32;
                params.repeat_penalty = Some(rp);
            }

            let v = obj.get_property_str(ctx, "repeatPenaltyWindow")?;
            if !v.is_undefined() {
                let rw = v.to_number(ctx)?.to_i32();
                params.repeat_penalty_window = Some(rw);
            }

            if !params.is_empty() {
                params_opt = Some(params);
            }
        }
    }

    let (promise, tx) =
        Promise::create_from_sender(ctx, |cx, payload: Result<i32, String>| -> JSResult {
            match payload {
                Ok(token) => Ok(token.into()),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;

    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(e) = WorkerPool::global().submit(
        move || {
            let res = get_sampler_spec_by_id(sampler_id).and_then(|sampler_spec| {
                state.sample_token(sampler_id, logits, sampler_spec, params_opt)
            });
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

fn reset(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let state = get_llama_context_state(ctx, &mut this)?;
    let (promise, tx) =
        Promise::create_from_sender(ctx, |cx, payload: Result<(), String>| -> JSResult {
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
    if let Err(e) = WorkerPool::global().submit(
        move || {
            let res = state.reset();
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

fn get_stats(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let state = get_llama_context_state(ctx, &mut this)?;
    let (promise, tx) = Promise::create_from_sender(
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
    if let Err(e) = WorkerPool::global().submit(
        move || {
            let res = state.get_stats();
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

fn dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let state = get_llama_context_state(ctx, &mut this)?;

    let _ = this.set_property_str(ctx, "model", JSValue::UNDEFINED);
    let (promise, tx) =
        Promise::create_from_sender(ctx, |_cx, payload: Result<(), String>| -> JSResult {
            match payload {
                Ok(()) => Ok(JSValue::UNDEFINED),
                Err(_) => Ok(JSValue::UNDEFINED),
            }
        })?;

    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();
    if let Err(_e) = WorkerPool::global().submit(
        move || {
            state.dispose();
            let _ = tx_submit.send(Ok(()));
        },
        Some(move || {
            let _ = tx_timeout.send(Ok(()));
        }),
    ) {
        let _ = tx.send(Ok(()));
    }

    Ok(promise.into())
}

const CLASS_CONTEXT_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("contextSize", Some(get_prop), None, PROP_CTX_SIZE as _),
    FieldDef::GETSET("batchSize", Some(get_prop), None, PROP_BATCH_SIZE as _),
    FieldDef::GETSET("isDisposed", Some(get_prop), None, PROP_IS_DISPOSED as _),
    FieldDef::CALL("decode", 2, decode, 0),
    FieldDef::CALL("sample", 2, sample, 0),
    FieldDef::CALL("reset", 0, reset, 0),
    FieldDef::CALL("getStats", 0, get_stats, 0),
    FieldDef::CALL("dispose", 0, dispose, 0),
];

pub struct JSLlamaContext;

impl JSLlamaContext {
    fn handle_dispose(_: &mut RuntimeImpl, opaque: *mut c_void, _: usize) {
        if !opaque.is_null() {
            drop(unsafe { Box::from_raw(opaque.cast::<Arc<LlamaContextState>>()) });
        }
    }

    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    /// register class
    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("Context", CLASS_CONTEXT_PROTO, None).on_dispose(Self::handle_dispose),
        )?;

        // save assigned class_id
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    /// get class id
    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub fn new_instance(ctx: &mut ScopedContext, llm_ctx: LlamaContext) -> Result<Object, i32> {
        let class_id = Self::class_id();
        let llm_ctx_static: LlamaContext<'static> = unsafe { std::mem::transmute(llm_ctx) };
        let state = Arc::new(LlamaContextState::new(llm_ctx_static));
        let opaque = Box::into_raw(Box::new(state)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(class_id, opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<Arc<LlamaContextState>>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_context(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaContext::register(rt)
}
