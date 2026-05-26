mod class_client;
mod class_client_response;
mod class_context;
mod class_engine;
mod class_model;
mod class_sampler;

pub(crate) use {
    class_client::JSLlamaClient,
    class_context::JSLlamaContext,
    class_engine::{ERR_CANCELLED, ERR_ENGINE_SHUTDOWN, JSEngine},
    class_model::JSLlamaModel,
    class_sampler::JSSamplerChain,
};

use llama_cpp_ffi::{FileType, KVCacheType, list_devices};
use zuqe::{
    Array, CallContext, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue, NativeObject,
    ScopedContext,
};

fn get_devices(ctx: &mut ScopedContext, _: CallContext) -> JSResult {
    let devs = list_devices();

    let mut lst = Vec::<JSValue>::with_capacity(devs.len());
    for d in devs {
        if let Some(d) = d {
            let mut o = ctx.create_base_object()?;

            let ty: JSValue = ctx.create_string_from_static(d.dev_type.to_str())?.into();
            o.define_property_value_str(ctx, "type", ty, JS_PROP_ENUMERABLE)?;

            let name: JSValue = ctx.create_string_from_utf8(d.name.into_bytes())?.into();
            o.define_property_value_str(ctx, "name", name, JS_PROP_ENUMERABLE)?;

            lst.push(o.into());
        } else {
            lst.push(JSValue::NULL);
        }
    }

    Array::from_values(ctx, lst).map(JSValue::from)
}

const MOD_LLAMA_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        "Model",
        || class_model::JSLlamaModel::class_id(),
        JS_PROP_ENUMERABLE,
    ),
    FieldDef::CLASS(
        "Client",
        || class_client::JSLlamaClient::class_id(),
        JS_PROP_ENUMERABLE,
    ),
    FieldDef::OBJECT(
        "FileType",
        &[
            FieldDef::INT32("ALL_F32", FileType::AllF32 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_F16", FileType::MostlyF16 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_Q4_0", FileType::MostlyQ4_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_Q4_1", FileType::MostlyQ4_1 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_Q8_0", FileType::MostlyQ8_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_Q5_0", FileType::MostlyQ5_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_Q5_1", FileType::MostlyQ5_1 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("MOSTLY_Q2_K", FileType::MostlyQ2_K as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32(
                "MOSTLY_Q3_K_S",
                FileType::MostlyQ3_K_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q3_K_M",
                FileType::MostlyQ3_K_M as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q3_K_L",
                FileType::MostlyQ3_K_L as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q4_K_S",
                FileType::MostlyQ4_K_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q4_K_M",
                FileType::MostlyQ4_K_M as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q5_K_S",
                FileType::MostlyQ5_K_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q5_K_M",
                FileType::MostlyQ5_K_M as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32("MOSTLY_Q6_K", FileType::MostlyQ6_K as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32(
                "MOSTLY_IQ2_XXS",
                FileType::MostlyIQ2_XXS as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ2_XS",
                FileType::MostlyIQ2_XS as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_Q2_K_S",
                FileType::MostlyQ2_K_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ3_XS",
                FileType::MostlyIQ3_XS as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ3_XXS",
                FileType::MostlyIQ3_XXS as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ1_S",
                FileType::MostlyIQ1_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ4_NL",
                FileType::MostlyIQ4_NL as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ3_S",
                FileType::MostlyIQ3_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ3_M",
                FileType::MostlyIQ3_M as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ2_S",
                FileType::MostlyIQ2_S as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ2_M",
                FileType::MostlyIQ2_M as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ4_XS",
                FileType::MostlyIQ4_XS as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_IQ1_M",
                FileType::MostlyIQ1_M as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32("MOSTLY_BF16", FileType::MostlyBF16 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32(
                "MOSTLY_TQ1_0",
                FileType::MostlyTQ1_0 as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_TQ2_0",
                FileType::MostlyTQ2_0 as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_MXFP4_MOE",
                FileType::MostlyMXFP4_MOE as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32(
                "MOSTLY_NVFP4",
                FileType::MostlyNVFP4 as _,
                JS_PROP_ENUMERABLE,
            ),
            FieldDef::INT32("MOSTLY_Q1_0", FileType::MostlyQ1_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("GUESSED", FileType::Guessed as _, JS_PROP_ENUMERABLE),
        ],
        JS_PROP_ENUMERABLE,
    ),
    FieldDef::OBJECT(
        "KVCacheType",
        &[
            FieldDef::INT32("F32", KVCacheType::F32 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("F16", KVCacheType::F16 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("BF16", KVCacheType::BF16 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("Q8_0", KVCacheType::Q8_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("Q4_0", KVCacheType::Q4_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("Q4_1", KVCacheType::Q4_1 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("IQ4_NL", KVCacheType::IQ4_NL as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("Q5_0", KVCacheType::Q5_0 as _, JS_PROP_ENUMERABLE),
            FieldDef::INT32("Q5_1", KVCacheType::Q5_1 as _, JS_PROP_ENUMERABLE),
        ],
        JS_PROP_ENUMERABLE,
    ),
    FieldDef::FUNC("getDevices", 0, get_devices, 0),
];

pub fn init_mod_llama(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("llama", MOD_LLAMA_ENTRIES)?;
    Ok(())
}
