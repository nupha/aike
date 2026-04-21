use std::{
    cell::Cell,
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use linkme::distributed_slice;
use llama_cpp_ffi::{SamplerKind, SamplingParams};
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, IntOrFloat, JS_PROP_ENUMERABLE, JSResult, JSValue,
    Object, Runtime, RuntimeImpl, ScopedContext,
};

use crate::{USER_CLASS_DEFS, class_llama_model::get_llama_model};

pub(super) type SamplerId = usize;

#[derive(Clone, Default)]
pub(super) struct SamplerSpec {
    pub version: u64,
    pub steps: Vec<SamplerKind>,
    pub defaults: SamplingParams,
    pub vocab_ptr: usize,
    pub ctx_train: i32,
}

static SAMPLER_SPEC_LUT: OnceLock<Mutex<HashMap<SamplerId, SamplerSpec>>> = OnceLock::new();

fn sampler_spec_lut() -> &'static Mutex<HashMap<SamplerId, SamplerSpec>> {
    SAMPLER_SPEC_LUT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// retrive sampler id from js value
pub(super) fn get_sampler_id(ctx: &mut ScopedContext, val: &JSValue) -> Result<SamplerId, i32> {
    let obj = val.as_object().ok_or_else(|| {
        ctx.throw_type_error("invalid Sampler object");
        -1
    })?;
    let sampler_id = obj.id();
    let exists = sampler_spec_lut()
        .lock()
        .map_err(|_| {
            ctx.throw_internal_error("sampler registry poisoned");
            -1
        })?
        .contains_key(&sampler_id);
    if exists {
        Ok(sampler_id)
    } else {
        ctx.throw_type_error("invalid Sampler object");
        Err(-1)
    }
}

fn get_sampler_spec_mut<'a>(
    ctx: &mut ScopedContext,
    reg: &'a mut HashMap<SamplerId, SamplerSpec>,
    sampler_id: SamplerId,
) -> Result<&'a mut SamplerSpec, i32> {
    if let Some(spec) = reg.get_mut(&sampler_id) {
        Ok(spec)
    } else {
        ctx.throw_type_error("invalid Sampler object");
        Err(-1)
    }
}

pub(super) fn get_sampler_spec_by_id(sampler_id: SamplerId) -> Result<SamplerSpec, String> {
    let reg = sampler_spec_lut()
        .lock()
        .map_err(|_| "sampler registry poisoned".to_string())?;
    reg.get(&sampler_id)
        .cloned()
        .ok_or_else(|| "Sampler was disposed".to_string())
}

pub fn js_model_createSamplerChain(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let mut spec = SamplerSpec {
        version: 1,
        steps: Vec::new(),
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

                        spec.defaults.repeat_penalty_window = Some(repeat_last_n);
                        spec.defaults.repeat_penalty = Some(repeat_penalty);
                        spec.steps.push(SamplerKind::Penalties {
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
                        spec.defaults.top_k = Some(d);
                        spec.steps.push(SamplerKind::TopK(d));
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
                        spec.defaults.top_p = Some(d);
                        spec.steps.push(SamplerKind::TopP(d));
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

                        spec.defaults.temperature = Some(temp);
                        spec.steps.push(SamplerKind::Temperature {
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

    spec.steps.push(SamplerKind::Dist(0));
    let mut obj = JSSamplerChain::new_instance(ctx)?;
    let sampler_id = obj.id();
    sampler_spec_lut()
        .lock()
        .map_err(|_| {
            ctx.throw_internal_error("sampler registry poisoned");
            -1
        })?
        .insert(sampler_id, spec);
    obj.define_property_value_str(ctx, "model", this, JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

fn add_penalties(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sampler_id = get_sampler_id(ctx, &this)?;
    let mut reg = sampler_spec_lut().lock().map_err(|_| {
        ctx.throw_internal_error("sampler registry poisoned");
        -1
    })?;
    let spec = get_sampler_spec_mut(ctx, &mut reg, sampler_id)?;

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

    spec.defaults.repeat_penalty_window = Some(repeat_last_n);
    spec.defaults.repeat_penalty = Some(repeat_penalty);
    spec.steps.push(SamplerKind::Penalties {
        repeat_last_n,
        repeat_penalty,
        frequency_penalty,
        presence_penalty,
    });
    spec.version = spec.version.saturating_add(1);

    Ok(cc.this())
}

const TOP_K: i32 = 1;
const TOP_P: i32 = 2;

fn add_top_k_p(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sampler_id = get_sampler_id(ctx, &this)?;
    let mut reg = sampler_spec_lut().lock().map_err(|_| {
        ctx.throw_internal_error("sampler registry poisoned");
        -1
    })?;
    let spec = get_sampler_spec_mut(ctx, &mut reg, sampler_id)?;

    let n = cc.arg(0).to_number(ctx)?;
    match cc.magic() {
        TOP_K => {
            let d = n.to_i32();
            spec.defaults.top_k = Some(d);
            spec.steps.push(SamplerKind::TopK(d));
            spec.steps.push(SamplerKind::Dist(0));
        }
        TOP_P => {
            let d = n.to_f64() as f32;
            spec.defaults.top_p = Some(d);
            spec.steps.push(SamplerKind::TopP(d));
        }
        _ => {
            unreachable!()
        }
    }
    spec.version = spec.version.saturating_add(1);

    Ok(cc.this())
}

fn add_temperature(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sampler_id = get_sampler_id(ctx, &this)?;
    let mut reg = sampler_spec_lut().lock().map_err(|_| {
        ctx.throw_internal_error("sampler registry poisoned");
        -1
    })?;
    let spec = get_sampler_spec_mut(ctx, &mut reg, sampler_id)?;

    let d = cc.arg(0).to_number(ctx)?.to_f64();
    spec.defaults.temperature = Some(d as f32);
    spec.steps.push(SamplerKind::Temperature {
        temp: d as f32,
        dyn_range: 1.0,
        dyn_exponent: 1.0,
    });
    spec.version = spec.version.saturating_add(1);

    Ok(cc.this())
}

const CLASS_SAMPLER_CHAIN_PROTO: &[FieldDef] = &[
    FieldDef::CALL("penalties", 1, add_penalties, 0),
    FieldDef::CALL("topK", 1, add_top_k_p, TOP_K as _),
    FieldDef::CALL("topP", 1, add_top_k_p, TOP_P as _),
    FieldDef::CALL("temperature", 1, add_temperature, 0 as _),
];

pub struct JSSamplerChain;

impl JSSamplerChain {
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

    fn handle_dispose(_: &mut RuntimeImpl, _: *mut std::ffi::c_void, id: usize) {
        if let Ok(mut reg) = sampler_spec_lut().lock() {
            reg.remove(&id);
        }
    }

    /// get class id
    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub fn new_instance(ctx: &mut ScopedContext) -> Result<Object, i32> {
        let class_id = Self::class_id();
        ctx.create_class_instance_with_opaque(class_id, std::ptr::null_mut())
            .map_err(|_| -1)
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_sampler(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSSamplerChain::register(rt)
}
