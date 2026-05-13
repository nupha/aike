use std::{
    cell::Cell,
    ffi::c_void,
    sync::{Arc, Mutex},
};

use linkme::distributed_slice;
use llama_cpp_ffi::{SamplerChain, SamplerKind};
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, IntOrFloat, JS_PROP_ENUMERABLE, JSResult, JSValue,
    Object, Runtime, RuntimeImpl, ScopedContext,
};

use crate::{USER_CLASS_DEFS, class_llama_model::get_llama_model};

fn get_sampler_chain<'a>(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<&'a Mutex<SamplerChain>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSSamplerChain::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("SamplerChain was disposed");
        Err(-1)
    } else {
        let k = unsafe { &*opaque.cast::<Mutex<SamplerChain>>() };
        Ok(k)
    }
}

pub(super) fn get_sampler_chain_arc(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Arc<Mutex<SamplerChain>>, i32> {
    let chain_ref = get_sampler_chain(ctx, val)?;
    let ptr = chain_ref as *const Mutex<SamplerChain>;
    unsafe { Arc::increment_strong_count(ptr) };
    Ok(unsafe { Arc::from_raw(ptr) })
}

pub fn js_model_createSamplerChain(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    let mut chain = SamplerChain::new(model, false);

    if cc.length() > 0 {
        let arr = cc.arg(0).as_object_Array().copied().ok_or_else(|| {
            ctx.throw_type_error("params must be array");
            -1
        })?;

        for v in arr.payload().values() {
            if let Some(o) = v.as_object() {
                let kind = o
                    .get_property_str(ctx, "kind")?
                    .as_string()
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

                        match o.get_property_str(ctx, "repeatLast")? {
                            JSValue::UNDEFINED => {}
                            v => {
                                if let IntOrFloat::Int64(n) = v.to_number(ctx)?
                                    && (0..i32::MAX as i64).contains(&n)
                                {
                                    repeat_last_n = n as i32;
                                } else {
                                    ctx.throw_type_error("invalid repeatLast");
                                    return Err(-1);
                                }
                            }
                        }

                        match o.get_property_str(ctx, "repeatPenalty")? {
                            JSValue::UNDEFINED => {}
                            v => {
                                let d = v.to_number(ctx)?.to_f64();
                                if (0.0..f32::MAX as f64).contains(&d) {
                                    repeat_penalty = d as _;
                                } else {
                                    ctx.throw_type_error("invalid repeatPenalty");
                                    return Err(-1);
                                }
                            }
                        }

                        match o.get_property_str(ctx, "frequencyPenalty")? {
                            JSValue::UNDEFINED => {}
                            v => {
                                let d = v.to_number(ctx)?.to_f64();
                                if (0.0..f32::MAX as f64).contains(&d) {
                                    frequency_penalty = d as _;
                                } else {
                                    ctx.throw_type_error("invalid frequencyPenalty");
                                    return Err(-1);
                                }
                            }
                        }

                        match o.get_property_str(ctx, "presencePenalty")? {
                            JSValue::UNDEFINED => {}
                            v => {
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

    let mut obj = JSSamplerChain::new_instance(ctx, Arc::new(Mutex::new(chain)))?;
    obj.define_property_value_str(ctx, "model", this, JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

fn js_samplerchain_penalties(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chain = get_sampler_chain(ctx, &this)?;
    let mut chain = chain.lock().unwrap();

    let mut repeat_last_n = 64_i32;
    let mut repeat_penalty = 1.1;
    let mut frequency_penalty = 0.0;
    let mut presence_penalty = 0.0;

    if cc.length() > 0 {
        let opts = cc.arg(0).as_object().cloned().ok_or_else(|| {
            ctx.throw_type_error("expect object");
            -1
        })?;

        match opts.get_property_str(ctx, "repeatLastN")? {
            JSValue::UNDEFINED => {}
            v => {
                if let IntOrFloat::Int64(n) = v.to_number(ctx)?
                    && (0..i32::MAX as i64).contains(&n)
                {
                    repeat_last_n = n as i32;
                } else {
                    ctx.throw_type_error("invalid repeatLastN");
                    return Err(-1);
                }
            }
        }

        match opts.get_property_str(ctx, "repeatPenalty")? {
            JSValue::UNDEFINED => {}
            v => {
                let d = v.to_number(ctx)?.to_f64();
                if (0.0..f32::MAX as f64).contains(&d) {
                    repeat_penalty = d as _;
                } else {
                    ctx.throw_type_error("invalid repeatPenalty");
                    return Err(-1);
                }
            }
        }

        match opts.get_property_str(ctx, "frequencyPenalty")? {
            JSValue::UNDEFINED => {}
            v => {
                let d = v.to_number(ctx)?.to_f64();
                if (0.0..f32::MAX as f64).contains(&d) {
                    frequency_penalty = d as _;
                } else {
                    ctx.throw_type_error("invalid frequencyPenalty");
                    return Err(-1);
                }
            }
        }

        match opts.get_property_str(ctx, "presencePenalty")? {
            JSValue::UNDEFINED => {}
            v => {
                let d = v.to_number(ctx)?.to_f64();
                if (0.0..f32::MAX as f64).contains(&d) {
                    presence_penalty = d as _;
                } else {
                    ctx.throw_type_error("invalid presencePenalty");
                    return Err(-1);
                }
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
    let chain = get_sampler_chain(ctx, &this)?;
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
    let chain = get_sampler_chain(ctx, &this)?;
    let mut chain = chain.lock().unwrap();

    let d = cc.arg(0).to_number(ctx)?.to_f64();
    chain.add_sampler(&SamplerKind::Temperature {
        temp: d as f32,
        dyn_range: 1.0,
        dyn_exponent: 1.0,
    });

    Ok(this)
}

fn add_grammar(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let chain = get_sampler_chain(ctx, &this)?;
    let mut chain = chain.lock().unwrap();

    let g = cc.arg(0);
    if g.is_undefined_or_null() {
        ctx.throw_type_error("grammar should be string");
        return Err(-1);
    }
    let s = g.to_string(ctx)?;
    if s.is_empty() {
        ctx.throw_type_error("empty grammar");
        return Err(-1);
    }

    let lark = s.content().encode_utf8(false);
    if let Err(e) = chain.set_grammar(&lark) {
        log::warn!("failed to set grammar: {}", e);
    }

    Ok(this)
}

#[cfg(debug_assertions)]
mod debug_api {
    use super::*;

    pub(super) fn debug_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
        let this = cc.this();
        let _guard = get_sampler_chain(ctx, &this)?.lock().unwrap();
        let mut this = cc.this();
        let payload = this
            .retrive_object_payload_mut(ctx, JSSamplerChain::class_id())?
            .other_mut()
            .unwrap();
        if !payload.opaque().is_null() {
            unsafe {
                let p = payload.replace_opaque(std::ptr::null_mut());
                drop(Arc::from_raw(p.cast::<Mutex<SamplerChain>>()));
            }
        }
        Ok(JSValue::UNDEFINED)
    }

    pub(super) fn debug_cache_count(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
        let this = cc.this();
        let chain = get_sampler_chain(ctx, &this)?;
        let chain = chain.lock().unwrap();
        let n = chain.len();
        Ok((n as i32).into())
    }
}

const CLASS_SAMPLER_CHAIN_PROTO: &[FieldDef] = &[
    FieldDef::CALL("penalties", 1, js_samplerchain_penalties, 0),
    FieldDef::CALL("topK", 1, js_samplerchain_topKP, TOP_K as _),
    FieldDef::CALL("topP", 1, js_samplerchain_topKP, TOP_P as _),
    FieldDef::CALL("temperature", 1, js_samplerchain_temperature, 0 as _),
    #[cfg(debug_assertions)]
    FieldDef::CALL("_debug_dispose", 0, debug_api::debug_dispose, 0),
    #[cfg(debug_assertions)]
    FieldDef::CALL("_debug_cacheCount", 0, debug_api::debug_cache_count, 0),
];

pub struct JSSamplerChain;

impl JSSamplerChain {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("SamplerChain", CLASS_SAMPLER_CHAIN_PROTO, None)
                .on_dispose(Self::handle_dispose),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    fn handle_dispose(_: &mut RuntimeImpl, opaque: *mut c_void, _id: usize) {
        if !opaque.is_null() {
            drop(unsafe { Arc::from_raw(opaque.cast::<Mutex<SamplerChain>>()) });
        }
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub fn new_instance(
        ctx: &mut ScopedContext,
        chain: Arc<Mutex<SamplerChain>>,
    ) -> Result<Object, i32> {
        let opaque = Arc::into_raw(chain) as *mut c_void;

        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Arc::from_raw(p.opaque().cast::<Mutex<SamplerChain>>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_sampler(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSSamplerChain::register(rt)
}
