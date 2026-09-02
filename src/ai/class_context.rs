// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#![cfg(feature = "llama_cpp")]

use std::{
    borrow::Cow,
    cell::Cell,
    sync::{Arc, Mutex},
    time::Duration,
};

use linkme::distributed_slice;
use llama::LlamaContext;
use zuqe::{
    CallContext, ClassID, FieldDef, IntOrFloat, JS_PROP_ENUMERABLE, JS_PROP_FORCE, JSResult,
    JSValue, Runtime, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

pub(crate) struct LlamaContextPayload(pub(crate) Arc<Mutex<Option<LlamaContext>>>);

impl std::ops::Deref for LlamaContextPayload {
    type Target = Arc<Mutex<Option<LlamaContext>>>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl GcTrace for LlamaContextPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

use crate::{USER_CLASS_DEFS, worker::WorkerPool};

use super::{
    class_engine::{JSEngine, parse_engine_params},
    class_sampler::get_sampler,
    engine::{Engine, Token},
};

pub(super) fn parse_tokens<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<Cow<'a, [i32]>, i32> {
    if let Some(ta) = val.as_object_TypedArray() {
        if let Some(lst) = ta.as_i32_array() {
            Ok(Cow::Owned(lst.to_vec()))
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
    } else if let Some(i) = val.as_int() {
        Ok(Cow::Owned(vec![i]))
    } else {
        ctx.throw_type_error("invalid input token");
        Err(-1)
    }
}

const PROP_CTX_SIZE: i32 = 1;
const PROP_BATCH_SIZE: i32 = 2;
const PROP_SEQUENCES: i32 = 3;
const PROP_IS_DISPOSED: i32 = 4;

fn get_llm_ctx(
    ctx: &mut ScopedContext,
    val: JSValue,
) -> Result<Arc<Mutex<Option<LlamaContext>>>, i32> {
    #[cfg(debug_assertions)]
    {
        JSLlmContext::try_from(val)
            .unwrap()
            .payload()
            .map(|p| {
                debug_assert!(!p.lock().unwrap().is_none());
                p.clone()
            })
            .ok_or_else(|| {
                ctx.throw_type_error("LLM Context was detached");
                -1
            })
    }

    #[cfg(not(debug_assertions))]
    {
        JSLlmContext::try_from(val)
            .unwrap()
            .payload()
            .cloned()
            .ok_or_else(|| {
                ctx.throw_type_error("LLM Context was detached");
                -1
            })
    }
}

fn take_llm_ctx(
    ctx: &mut ScopedContext,
    mut this: JSValue,
) -> Result<Option<(LlamaContext, JSValue)>, i32> {
    if let Some(llm_ctx) = JSLlmContext::take_payload_of(&mut this) {
        let lc = llm_ctx.lock().unwrap().take();
        debug_assert!(lc.is_some());

        let model_val = this.get_property_str(ctx, "model")?;
        this.delete_property_str(ctx, "model", JS_PROP_FORCE)?; // force delete

        Ok(Some((lc.unwrap(), model_val)))
    } else {
        Ok(None)
    }
}

fn js_context_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    if cc.magic() == PROP_IS_DISPOSED {
        return Ok(JSLlmContext::payload_of(&this).is_none().into());
    }

    let llm_ctx = get_llm_ctx(ctx, this)?;
    let llm = llm_ctx.lock().unwrap();
    let lc = llm.as_ref().unwrap();

    let val: JSValue = match cc.magic() {
        PROP_CTX_SIZE => lc.n_ctx().into(),
        PROP_BATCH_SIZE => lc.n_ubatch().into(),
        PROP_SEQUENCES => lc.n_seq_max().into(),
        _ => JSValue::UNDEFINED,
    };
    Ok(val)
}

fn js_context_decode(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let llm_ctx = get_llm_ctx(ctx, this)?;

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
        if !arg2.is_undefined() {
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
            //let res = llm_ctx.decode_tokens(tokens, n_past, logits_last);

            let res = {
                let seq_id = 0;
                let len = tokens.len();
                let mut logits_index: Option<i32> = None;

                let mut llm = llm_ctx.lock().unwrap();
                let llm = llm.as_mut().unwrap();
                let batch = llm.batch_mut();

                for (i, &t) in tokens.iter().enumerate() {
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
                llm.decode().map(|_| logits_index)
            };

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
    let this = cc.this();
    let llm_ctx = get_llm_ctx(ctx, this)?;

    let logits = cc.arg(0).to_number(ctx)?.to_i32();
    let s = cc.arg(1);
    let sampler = get_sampler(ctx, &s)?;

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
            // let res = llm_ctx.sample_token(logits, sampler.lock().unwrap().deref_mut());
            let res = {
                let llm = llm_ctx.lock().unwrap();
                let token = sampler
                    .lock()
                    .unwrap()
                    .sample(llm.as_ref().unwrap().as_ptr(), logits);
                Ok(token)
            };
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
    let llm_ctx = get_llm_ctx(ctx, cc.this())?;

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
            llm_ctx.lock().unwrap().as_mut().unwrap().clear_memory(true);
            let _ = tx_submit.send(Ok(()));
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
    #[derive(Clone, Copy)]
    struct StatsData {
        memory_used: i64,
        kv_cache_size: i64,
        used_tokens: i64,
        max_tokens: i64,
    }

    let llm_ctx = get_llm_ctx(ctx, cc.this())?;

    let (promise, tx) =
        Promise::create_with_channel(ctx, |cx, payload: Result<StatsData, String>| -> JSResult {
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
        })?;

    let tx_submit = tx.clone();
    let tx_timeout = tx.clone();

    if let Err(e) = WorkerPool::singleton().add_job(
        move || {
            let stat = {
                let lc = llm_ctx.lock().unwrap();
                let lc = lc.as_ref().unwrap();

                let memory_used = lc.state_size() as i64;
                let kv_cache_size = lc.n_ctx_seq() as i64;
                let pos_max = lc.memory_seq_pos_max(0);
                let used_tokens = if pos_max >= 0 { pos_max as i64 + 1 } else { 0 };
                let max_tokens = lc.n_ctx_seq() as i64;

                StatsData {
                    memory_used,
                    kv_cache_size,
                    used_tokens,
                    max_tokens,
                }
            };
            let _ = tx_submit.send(Ok(stat));
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
    let llm_ctx = get_llm_ctx(ctx, cc.this())?;

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
            let res = llm_ctx
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .embed_tokens(&tokens, 0);

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

fn js_context_createEngine(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // Parse engine options first, before taking the LlamaContext.
    let opts = if cc.length() > 0 {
        let v = cc.arg(0);
        if v.is_undefined() || v.is_null() {
            None
        } else {
            Some(v.as_object().ok_or_else(|| {
                ctx.throw_type_error("engineOptions must be object");
                -1
            })?)
        }
    } else {
        None
    };
    let eng_params = parse_engine_params(ctx, opts.as_ref())?;

    // take LlamaContext from the `this` object
    if let Some((llm_ctx, model_val)) = take_llm_ctx(ctx, cc.this())? {
        if llm_ctx.n_seq_max() == 1 {
            log::warn!("createEngine: context sequences is 1, only one parallel slot available");
        }

        let engine = Engine::start(
            llm_ctx,
            Duration::from_millis(eng_params.idle_sleep_ms),
            eng_params.slot_match_threshold,
            eng_params.max_tokens,
            eng_params.n_cache_reuse,
            eng_params.cache_ram_mb * 1024 * 1024,
        );

        let mut engine_obj = JSEngine::new_instance(ctx, JSValue::UNDEFINED, engine)?;
        let _ = engine_obj.define_property_value_str(ctx, "model", model_val, JS_PROP_ENUMERABLE);

        Ok(engine_obj.into())
    } else {
        ctx.throw_type_error("Context was detached");
        Err(-1)
    }
}

fn js_context_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    take_llm_ctx(ctx, cc.this())?;
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

def_native_object!(
    "Context",
    JSLlmContext,
    LlamaContextPayload,
    CLASS_CONTEXT_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_context(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlmContext::register(rt)
}
