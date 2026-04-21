use std::{borrow::Cow, cell::Cell, ffi::c_void};

use linkme::distributed_slice;
use llama_cpp_ffi::{LlamaContext, SamplingParams};
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, IntOrFloat, JSResult, JSValue, Object, Runtime,
    RuntimeImpl, ScopedContext,
};

use crate::{USER_CLASS_DEFS, class_llama_sampler::get_sampler};

const PROP_CTX_SIZE: i32 = 1;
const PROP_BATCH_SIZE: i32 = 2;
const PROP_SEQUENCES: i32 = 3;
const PROP_IS_DISPOSED: i32 = 4;

fn get_llama_ctx<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut LlamaContext<'a>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaContext::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();

    if opaque.is_null() {
        ctx.throw_type_error("Context was disposed");
        Err(-1)
    } else {
        let ctx_ref = unsafe { &mut *opaque.cast::<LlamaContext>() };
        Ok(ctx_ref)
    }
}

fn get_prop(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    if cc.magic() == PROP_IS_DISPOSED {
        let payload = this
            .retrive_object_payload(ctx, JSLlamaContext::class_id())?
            .other()
            .unwrap();
        let disposed = payload.opaque().is_null();
        return Ok(JSValue::Bool(disposed));
    }

    let llm_ctx = get_llama_ctx(ctx, &mut this)?;

    let val: JSValue = match cc.magic() {
        PROP_CTX_SIZE => llm_ctx.n_ctx().into(),
        PROP_BATCH_SIZE => llm_ctx.n_ubatch().into(),
        PROP_SEQUENCES => llm_ctx.n_seq_max().into(),
        _ => JSValue::UNDEFINED,
    };

    Ok(val)
}

fn decode(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx = get_llama_ctx(ctx, &mut this)?;

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

    let seq_id = 0;
    let len = data.len();
    let batch = llm_ctx.batch_mut();

    let mut logits_index: Option<i32> = None;

    for (i, &t) in data.iter().enumerate() {
        unsafe {
            *batch.token.add(i) = t;
            *batch.pos.add(i) = n_past + i as i32;
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

    llm_ctx.decode().map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    if let Some(idx) = logits_index {
        Ok(idx.into())
    } else {
        Ok(JSValue::NULL)
    }
}

fn sample(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx = get_llama_ctx(ctx, &mut this)?;

    let logits = cc.arg(0).to_number(ctx)?.to_i32();
    let s = cc.arg(1);
    let sampler = get_sampler(ctx, &s)?;
    let current = sampler.current_sampling_params();

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
                if current.temperature != Some(t) {
                    params.temperature = Some(t);
                }
            }

            let v = obj.get_property_str(ctx, "topP")?;
            if !v.is_undefined() {
                let p = v.to_number(ctx)?.to_f64() as f32;
                if current.top_p != Some(p) {
                    params.top_p = Some(p);
                }
            }

            let v = obj.get_property_str(ctx, "topK")?;
            if !v.is_undefined() {
                let k = v.to_number(ctx)?.to_i32();
                if current.top_k != Some(k) {
                    params.top_k = Some(k);
                }
            }

            let v = obj.get_property_str(ctx, "repeatPenalty")?;
            if !v.is_undefined() {
                let rp = v.to_number(ctx)?.to_f64() as f32;
                if current.repeat_penalty != Some(rp) {
                    params.repeat_penalty = Some(rp);
                }
            }

            let v = obj.get_property_str(ctx, "repeatPenaltyWindow")?;
            if !v.is_undefined() {
                let rw = v.to_number(ctx)?.to_i32();
                if current.repeat_penalty_window != Some(rw) {
                    params.repeat_penalty_window = Some(rw);
                }
            }

            if !params.is_empty() {
                params_opt = Some(params);
            }
        }
    }

    let token = sampler.sample_with_params(llm_ctx, logits, params_opt.as_ref());
    Ok(token.into())
}

fn reset(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx = get_llama_ctx(ctx, &mut this)?;
    llm_ctx.clear_memory(true);
    Ok(JSValue::UNDEFINED)
}

fn get_stats(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let llm_ctx = get_llama_ctx(ctx, &mut this)?;

    let memory_used = llm_ctx.state_size() as i64;
    let kv_cache_size = llm_ctx.n_ctx_seq() as i64;
    let pos_max = llm_ctx.memory_seq_pos_max(0);
    let used_tokens = if pos_max >= 0 { pos_max as i64 + 1 } else { 0 };
    let max_tokens = llm_ctx.n_ctx_seq() as i64;

    let mut obj = ctx.create_base_object()?;
    obj.set_property_str(ctx, "memoryUsed", memory_used.into())?;
    obj.set_property_str(ctx, "kvCacheSize", kv_cache_size.into())?;
    obj.set_property_str(ctx, "usedTokens", used_tokens.into())?;
    obj.set_property_str(ctx, "maxTokens", max_tokens.into())?;
    Ok(obj.into())
}

fn dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let u = this
        .retrive_object_payload_mut(ctx, JSLlamaContext::class_id())?
        .other_mut()
        .unwrap();

    if !u.opaque().is_null() {
        unsafe {
            let p = u.replace_opaque(std::ptr::null_mut());
            drop(Box::from_raw(p.cast::<LlamaContext>()));
        }
    }

    let _ = this.set_property_str(ctx, "model", JSValue::UNDEFINED);

    Ok(JSValue::UNDEFINED)
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
            drop(unsafe { Box::from_raw(opaque.cast::<LlamaContext>()) });
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
        let opaque = Box::into_raw(Box::new(llm_ctx)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(class_id, opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<LlamaContext>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_context(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaContext::register(rt)
}
