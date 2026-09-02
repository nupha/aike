// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#![cfg(feature = "llama_cpp")]

use std::{
    cell::Cell,
    sync::{Arc, Mutex},
};

use linkme::distributed_slice;
use llama::{SamplerChain, SamplerKind};
use zuqe::{
    CallContext, ClassID, FieldDef, IntOrFloat, JS_PROP_ENUMERABLE, JSResult, JSValue, Runtime,
    ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

pub(crate) struct SamplerChainPayload(pub(crate) Arc<Mutex<SamplerChain>>);

impl std::ops::Deref for SamplerChainPayload {
    type Target = Arc<Mutex<SamplerChain>>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl GcTrace for SamplerChainPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

use crate::USER_CLASS_DEFS;

use super::class_model::get_llama_model;

pub(super) fn get_sampler(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Arc<Mutex<SamplerChain>>, i32> {
    let ob = JSSamplerChain::try_from(*val).unwrap();
    if let Some(p) = ob.payload() {
        Ok(p.clone())
    } else {
        ctx.throw_type_error("SamplerChain was detached");
        Err(-1)
    }
}

pub fn js_model_createSamplerChain(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let mut chain = SamplerChain::new(model.clone(), false);

    if cc.length() > 0 {
        let arr = cc.arg(0).as_object_Array().ok_or_else(|| {
            ctx.throw_type_error("params must be array");
            -1
        })?;

        for v in arr.payload().values() {
            if let Some(o) = v.as_object() {
                let kind = o
                    .get_property_str(ctx, "kind")?
                    .as_string(true)
                    .map(|s| s.content().encode_utf8(false).to_string())
                    .ok_or_else(|| {
                        ctx.throw_type_error("invalid sampler kind");
                        -1
                    })?;

                match kind.as_str() {
                    "penalties" => {
                        let mut repeat_last_n = 64_i32;
                        let mut repeat_penalty = 1.1;
                        let mut frequency_penalty = 0.0;
                        let mut presence_penalty = 0.0;

                        let v = o.get_property_str(ctx, "repeatLast")?;
                        if !v.is_undefined() {
                            if let IntOrFloat::Int64(n) = v.to_number(ctx)?
                                && (0..i32::MAX as i64).contains(&n)
                            {
                                repeat_last_n = n as i32;
                            } else {
                                ctx.throw_type_error("invalid repeatLast");
                                return Err(-1);
                            }
                        }

                        let v = o.get_property_str(ctx, "repeatPenalty")?;
                        if !v.is_undefined() {
                            let d = v.to_number(ctx)?.to_f64();
                            if (0.0..f32::MAX as f64).contains(&d) {
                                repeat_penalty = d as _;
                            } else {
                                ctx.throw_type_error("invalid repeatPenalty");
                                return Err(-1);
                            }
                        }

                        let v = o.get_property_str(ctx, "frequencyPenalty")?;
                        if !v.is_undefined() {
                            let d = v.to_number(ctx)?.to_f64();
                            if (0.0..f32::MAX as f64).contains(&d) {
                                frequency_penalty = d as _;
                            } else {
                                ctx.throw_type_error("invalid frequencyPenalty");
                                return Err(-1);
                            }
                        }

                        let v = o.get_property_str(ctx, "presencePenalty")?;
                        if !v.is_undefined() {
                            let d = v.to_number(ctx)?.to_f64();
                            if (0.0..f32::MAX as f64).contains(&d) {
                                presence_penalty = d as _;
                            } else {
                                ctx.throw_type_error("invalid presencePenalty");
                                return Err(-1);
                            }
                        }

                        chain.add_sampler(&SamplerKind::Penalties {
                            repeat_last_n,
                            repeat_penalty,
                            frequency_penalty,
                            presence_penalty,
                        });
                    }
                    "top-k" => {
                        let d = o
                            .get_property_str(ctx, "k")
                            .and_then(|v| v.to_number(ctx))
                            .map_err(|_| {
                                ctx.throw_type_error("invalid k");
                                -1
                            })?
                            .to_i32();
                        chain.add_sampler(&SamplerKind::TopK(d));
                    }
                    "top-p" => {
                        let d = o
                            .get_property_str(ctx, "p")
                            .and_then(|v| v.to_number(ctx))
                            .map_err(|_| {
                                ctx.throw_type_error("invalid p");
                                -1
                            })?
                            .to_f64() as f32;
                        chain.add_sampler(&SamplerKind::TopP(d));
                    }
                    "temperature" => {
                        let dyn_range = 0.0;
                        let dyn_exponent = 0.0;
                        let temp = o
                            .get_property_str(ctx, "temperature")
                            .and_then(|v| v.to_number(ctx))
                            .map_err(|_| {
                                ctx.throw_type_error("invalid temperature");
                                -1
                            })?
                            .to_f64() as f32;

                        chain.add_sampler(&SamplerKind::Temperature {
                            temp,
                            dyn_range,
                            dyn_exponent,
                        });
                    }
                    _ => {
                        ctx.throw_type_error("invalid sampler kind");
                        return Err(-1);
                    }
                }
            } else {
                ctx.throw_type_error_not_an_object();
                return Err(-1);
            }
        }
    }

    chain.add_sampler(&SamplerKind::Dist(0));
    chain.finalize_samplers();

    let mut obj =
        JSSamplerChain::new_instance(ctx, JSValue::UNDEFINED, Arc::new(Mutex::new(chain)))?;
    obj.define_property_value_str(ctx, "model", this, JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

fn js_samplerchain_penalties(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chain = get_sampler(ctx, &this)?;
    let mut chain = chain.lock().unwrap();

    let mut repeat_last_n = 64_i32;
    let mut repeat_penalty = 1.1;
    let mut frequency_penalty = 0.0;
    let mut presence_penalty = 0.0;

    if cc.length() > 0 {
        let opts = cc.arg(0).as_object().ok_or_else(|| {
            ctx.throw_type_error("expect object");
            -1
        })?;

        let v = opts.get_property_str(ctx, "repeatLastN")?;
        if !v.is_undefined() {
            if let IntOrFloat::Int64(n) = v.to_number(ctx)?
                && (0..i32::MAX as i64).contains(&n)
            {
                repeat_last_n = n as i32;
            } else {
                ctx.throw_type_error("invalid repeatLastN");
                return Err(-1);
            }
        }

        let v = opts.get_property_str(ctx, "repeatPenalty")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..f32::MAX as f64).contains(&d) {
                repeat_penalty = d as _;
            } else {
                ctx.throw_type_error("invalid repeatPenalty");
                return Err(-1);
            }
        }

        let v = opts.get_property_str(ctx, "frequencyPenalty")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..f32::MAX as f64).contains(&d) {
                frequency_penalty = d as _;
            } else {
                ctx.throw_type_error("invalid frequencyPenalty");
                return Err(-1);
            }
        }

        let v = opts.get_property_str(ctx, "presencePenalty")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..f32::MAX as f64).contains(&d) {
                presence_penalty = d as _;
            } else {
                ctx.throw_type_error("invalid presencePenalty");
                return Err(-1);
            }
        }
    }

    chain.add_sampler(&SamplerKind::Penalties {
        repeat_last_n,
        repeat_penalty,
        frequency_penalty,
        presence_penalty,
    });

    Ok(cc.this())
}

const TOP_K: i32 = 1;
const TOP_P: i32 = 2;

fn js_samplerchain_topKP(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chain = get_sampler(ctx, &this)?;
    let mut chain = chain.lock().unwrap();

    let n = cc.arg(0).to_number(ctx)?;
    match cc.magic() {
        TOP_K => {
            let d = n.to_i32();
            chain.add_sampler(&SamplerKind::TopK(d));
            chain.add_sampler(&SamplerKind::Dist(0));
        }
        TOP_P => {
            let d = n.to_f64() as f32;
            chain.add_sampler(&SamplerKind::TopP(d));
        }
        _ => {
            unreachable!()
        }
    }

    Ok(cc.this())
}

fn js_samplerchain_temperature(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chain = get_sampler(ctx, &this)?;
    let mut chain = chain.lock().unwrap();

    let d = cc.arg(0).to_number(ctx)?.to_f64();
    chain.add_sampler(&SamplerKind::Temperature {
        temp: d as f32,
        dyn_range: 1.0,
        dyn_exponent: 1.0,
    });

    Ok(this)
}

const CLASS_SAMPLER_CHAIN_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("penalties", 1, js_samplerchain_penalties, 0),
    FieldDef::FUNC("topK", 1, js_samplerchain_topKP, TOP_K as _),
    FieldDef::FUNC("topP", 1, js_samplerchain_topKP, TOP_P as _),
    FieldDef::FUNC("temperature", 1, js_samplerchain_temperature, 0 as _),
];

def_native_object!(
    "SamplerChain",
    JSSamplerChain,
    SamplerChainPayload,
    CLASS_SAMPLER_CHAIN_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_sampler(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSSamplerChain::register(rt)
}
