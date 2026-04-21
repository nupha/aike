use std::{cell::Cell, ffi::c_void};

use linkme::distributed_slice;
use llama_cpp_ffi::{SamplerKind, SamplingParams};
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, IntOrFloat, JS_PROP_ENUMERABLE, JSResult, JSValue,
    Object, Runtime, RuntimeImpl, ScopedContext,
};

use crate::{USER_CLASS_DEFS, class_llama_model::get_llama_model};

#[derive(Clone, Default)]
pub(super) struct SamplerConfig {
    pub kinds: Vec<SamplerKind>,
    pub defaults: SamplingParams,
    pub vocab_ptr: usize,
    pub ctx_train: i32,
}

pub(super) fn get_sampler_config(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<SamplerConfig, i32> {
    let opaque = val
        .retrive_object_payload(ctx, JSSamplerChain::class_id())?
        .other()
        .unwrap()
        .opaque();

    if !opaque.is_null() {
        Ok(unsafe { (&*opaque.cast::<SamplerConfig>()).clone() })
    } else {
        ctx.throw_type_error("invalid Sampler object");
        Err(-1)
    }
}

pub fn js_model_createSamplerChain(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let mut config = SamplerConfig {
        kinds: Vec::new(),
        defaults: SamplingParams::default(),
        vocab_ptr: model.vocab() as usize,
        ctx_train: model.n_ctx_train(),
    };

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

                        config.defaults.repeat_penalty_window = Some(repeat_last_n);
                        config.defaults.repeat_penalty = Some(repeat_penalty);
                        config.kinds.push(SamplerKind::Penalties {
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
                        config.defaults.top_k = Some(d);
                        config.kinds.push(SamplerKind::TopK(d));
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
                        config.defaults.top_p = Some(d);
                        config.kinds.push(SamplerKind::TopP(d));
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

                        config.defaults.temperature = Some(temp);
                        config.kinds.push(SamplerKind::Temperature {
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

    config.kinds.push(SamplerKind::Dist(0));

    let mut obj = JSSamplerChain::new_instance(ctx, config)?;
    obj.define_property_value_str(ctx, "model", this, JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

fn add_penalties(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let opaque = this
        .retrive_object_payload_mut(ctx, JSSamplerChain::class_id())?
        .other_mut()
        .unwrap()
        .opaque();
    if opaque.is_null() {
        ctx.throw_type_error("invalid Sampler object");
        return Err(-1);
    }
    let config = unsafe { &mut *opaque.cast::<SamplerConfig>() };

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

    config.defaults.repeat_penalty_window = Some(repeat_last_n);
    config.defaults.repeat_penalty = Some(repeat_penalty);
    config.kinds.push(SamplerKind::Penalties {
        repeat_last_n,
        repeat_penalty,
        frequency_penalty,
        presence_penalty,
    });

    Ok(this)
}

const TOP_K: i32 = 1;
const TOP_P: i32 = 2;

fn add_top_k_p(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let opaque = this
        .retrive_object_payload_mut(ctx, JSSamplerChain::class_id())?
        .other_mut()
        .unwrap()
        .opaque();
    if opaque.is_null() {
        ctx.throw_type_error("invalid Sampler object");
        return Err(-1);
    }
    let config = unsafe { &mut *opaque.cast::<SamplerConfig>() };

    let d = cc.arg(0).to_number(ctx)?.to_i32();
    match cc.magic() {
        TOP_K => {
            config.defaults.top_k = Some(d);
            config.kinds.push(SamplerKind::TopK(d as _));
            config.kinds.push(SamplerKind::Dist(0));
        }
        TOP_P => {
            config.defaults.top_p = Some(d as f32);
            config.kinds.push(SamplerKind::TopP(d as f32));
        }
        _ => {
            unreachable!()
        }
    }

    Ok(this)
}

fn add_temperature(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let opaque = this
        .retrive_object_payload_mut(ctx, JSSamplerChain::class_id())?
        .other_mut()
        .unwrap()
        .opaque();
    if opaque.is_null() {
        ctx.throw_type_error("invalid Sampler object");
        return Err(-1);
    }
    let config = unsafe { &mut *opaque.cast::<SamplerConfig>() };

    let d = cc.arg(0).to_number(ctx)?.to_f64();
    config.defaults.temperature = Some(d as f32);
    config.kinds.push(SamplerKind::Temperature {
        temp: d as f32,
        dyn_range: 1.0,
        dyn_exponent: 1.0,
    });

    Ok(this)
}

const CLASS_SAMPLER_CHAIN_PROTO: &[FieldDef] = &[
    FieldDef::CALL("penalties", 1, add_penalties, 0),
    FieldDef::CALL("topK", 1, add_top_k_p, TOP_K as _),
    FieldDef::CALL("topP", 1, add_top_k_p, TOP_P as _),
    FieldDef::CALL("temperature", 1, add_temperature, 0 as _),
];

pub struct JSSamplerChain;

impl JSSamplerChain {
    fn handle_dispose(_: &mut RuntimeImpl, opaque: *mut c_void, _: usize) {
        debug_assert!(!opaque.is_null());
        drop(unsafe { Box::from_raw(opaque.cast::<SamplerConfig>()) });
    }

    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    /// register class
    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("SamplerChain", CLASS_SAMPLER_CHAIN_PROTO, None)
                .on_dispose(Self::handle_dispose),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    /// get class id
    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub fn new_instance(ctx: &mut ScopedContext, config: SamplerConfig) -> Result<Object, i32> {
        let class_id = Self::class_id();
        let opaque = Box::into_raw(Box::new(config)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(class_id, opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<SamplerConfig>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_sampler(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSSamplerChain::register(rt)
}
