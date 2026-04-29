use std::{
    cell::Cell,
    ffi::{CStr, CString, c_void},
    sync::{Mutex, OnceLock},
};

use linkme::distributed_slice;
use llama_cpp_ffi::{
    AttentionType, BackendDevice, FlashAttentionType, KVCacheType, LlamaContext, LlamaModel,
    ModelOptions, PoolingType, RopeScalingType, SplitMode,
};
use serde_json::Value as JsonValue;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassDef, ClassID, FieldDef, JS_PROP_ENUMERABLE, JSResult,
    JSValue, Object, Runtime, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_context::{JSLlamaContext, parse_tokens},
    class_llama_sampler::js_model_createSamplerChain,
    helpers::{JinjaMessage, create_minijinja_env, preprocess_chat_template},
};

const MODEL_PATH_HIDDEN_KEY: &str = "__modelPath";
const MODEL_CHAT_TEMPLATE_PREPROCESSED_KEY: &str = "__chatTemplatePreprocessed";
static LLAMA_LOG_BUFFER: OnceLock<Mutex<String>> = OnceLock::new();

fn llama_log_buffer() -> &'static Mutex<String> {
    LLAMA_LOG_BUFFER.get_or_init(|| Mutex::new(String::new()))
}

fn clear_llama_log_buffer() {
    if let Ok(mut s) = llama_log_buffer().lock() {
        s.clear();
    }
}

fn append_llama_log(text: &str) {
    if let Ok(mut s) = llama_log_buffer().lock() {
        s.push_str(text);
        if s.len() > 16 * 1024 {
            let keep_from = s.len() - 16 * 1024;
            s.drain(..keep_from);
        }
    }
}

fn take_llama_log_buffer() -> String {
    if let Ok(mut s) = llama_log_buffer().lock() {
        let out = s.clone();
        s.clear();
        out
    } else {
        String::new()
    }
}

fn format_model_load_error(err: String) -> String {
    let logs = take_llama_log_buffer();
    if logs.trim().is_empty() {
        err
    } else {
        format!("{err}\nllama.cpp logs:\n{logs}")
    }
}

fn normalize_model_params_for_runtime(params: &mut llama_cpp_ffi::llama_model_params) {
    let mut device_count = 0usize;
    let n = unsafe { llama_cpp_ffi::ggml_backend_dev_count() };
    for i in 0..n {
        let dev = unsafe { llama_cpp_ffi::ggml_backend_dev_get(i) };
        if dev.is_null() {
            continue;
        }
        let ty = unsafe { llama_cpp_ffi::ggml_backend_dev_type(dev) };
        if matches!(
            BackendDevice::from_i32(ty as i32),
            Some(BackendDevice::Gpu | BackendDevice::Igpu)
        ) {
            device_count += 1;
        }
    }

    if device_count == 0 && params.main_gpu >= 0 {
        eprintln!(
            "[warn] no GPU backend device is available; forcing mainGpu = -1 (was {})",
            params.main_gpu
        );
        params.main_gpu = -1;
    }
}

fn parse_split_mode(ctx: &mut ScopedContext, val: &JSValue) -> Result<SplitMode, i32> {
    if let Some(s) = val.as_string() {
        return s
            .content()
            .encode_utf8(false)
            .as_ref()
            .parse::<SplitMode>()
            .ok()
            .ok_or_else(|| {
                ctx.throw_type_error("splitMode must be one of: none | layer | row | tensor");
                -1
            });
    }

    let n = val.to_number(ctx)?.to_i32();
    SplitMode::from_i32(n).ok_or_else(|| {
        ctx.throw_type_error("splitMode must be 0..3 or none/layer/row/tensor");
        -1
    })
}

fn parse_model_options(ctx: &mut ScopedContext, cc: &CallContext) -> Result<ModelOptions, i32> {
    let mut out = ModelOptions::default();

    if cc.length() <= 1 {
        return Ok(out);
    }

    let opts = cc.arg(1).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("model options must be object");
        -1
    })?;

    let v = opts.get_property_str(ctx, "gpuLayers")?;
    if !v.is_undefined() {
        if let Some(s) = v.as_string().and_then(|s| s.ascii()) {
            match s {
                b"auto" => out.gpu_layers = -1,
                b"all" => out.gpu_layers = -2,
                _ => {
                    ctx.throw_type_error("invalid gpuLayers");
                    return Err(-1);
                }
            }
        } else {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                out.gpu_layers = n;
            } else {
                ctx.throw_type_error("invalid gpuLayers");
                return Err(-1);
            }
        }
    }

    let v = opts.get_property_str(ctx, "splitMode")?;
    if !v.is_undefined() {
        out.split_mode = Some(parse_split_mode(ctx, &v)?);
    }

    let v = opts.get_property_str(ctx, "mainGpu")?;
    if !v.is_undefined() {
        let n = v.to_number(ctx)?.to_i32();
        if n < -1 {
            ctx.throw_type_error("mainGpu must be >= -1");
            return Err(-1);
        }
        out.main_gpu = Some(n);
    }

    let v = opts.get_property_str(ctx, "vocabOnly")?;
    if !v.is_undefined() {
        out.vocab_only = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "useMmap")?;
    if !v.is_undefined() {
        out.use_mmap = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "useDirectIo")?;
    if !v.is_undefined() {
        out.use_direct_io = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "useMlock")?;
    if !v.is_undefined() {
        out.use_mlock = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "checkTensors")?;
    if !v.is_undefined() {
        out.check_tensors = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "useExtraBufts")?;
    if !v.is_undefined() {
        out.use_extra_bufts = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "noHost")?;
    if !v.is_undefined() {
        out.no_host = Some(v.to_bool()?);
    }

    let v = opts.get_property_str(ctx, "noAlloc")?;
    if !v.is_undefined() {
        out.no_alloc = Some(v.to_bool()?);
    }

    // llama.cpp currently asserts on the combination of no_alloc + use_mmap.
    // If user enables noAlloc without explicitly disabling useMmap, ignore noAlloc.
    if out.no_alloc == Some(true) && out.use_mmap != Some(false) {
        eprintln!(
            "[warn] ModelOptions.noAlloc is ignored when useMmap is enabled; set useMmap: false to use noAlloc"
        );
        out.no_alloc = Some(false);
    }

    // CPU-only runtime compatibility:
    // llama.cpp rejects main_gpu >= 0 when there are no available devices.
    let no_gpu = {
        let mut c = 0usize;
        let n = unsafe { llama_cpp_ffi::ggml_backend_dev_count() };
        for i in 0..n {
            let dev = unsafe { llama_cpp_ffi::ggml_backend_dev_get(i) };
            if dev.is_null() {
                continue;
            }
            let ty = unsafe { llama_cpp_ffi::ggml_backend_dev_type(dev) };
            if matches!(
                BackendDevice::from_i32(ty as i32),
                Some(BackendDevice::Gpu | BackendDevice::Igpu)
            ) {
                c += 1;
            }
        }
        c == 0
    };
    if no_gpu {
        if out.main_gpu.is_some_and(|v| v >= 0) {
            eprintln!(
                "[warn] ModelOptions.mainGpu is ignored because no GPU device is available; forcing mainGpu = -1"
            );
        }
        out.main_gpu = Some(-1);
    }

    Ok(out)
}

fn js_model_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let file_name = v.content().encode_utf8(false).to_string();

    let mut model_params = LlamaModel::default_params();
    let model_opts = parse_model_options(ctx, &cc)?;
    model_opts.apply_to_params(&mut model_params);
    normalize_model_params_for_runtime(&mut model_params);
    clear_llama_log_buffer();

    let model =
        LlamaModel::load_from_file(&CString::new(file_name.as_str()).unwrap(), model_params)
            .map_err(|e| {
                ctx.throw_internal_error(&format_model_load_error(e));
                -1
            })?;

    let mut obj = JSLlamaModel::new_instance(ctx, model)?;
    let path = ctx
        .create_string_from_utf8(file_name.into_bytes())
        .map(JSValue::String)?;
    obj.define_property_value_str(ctx, MODEL_PATH_HIDDEN_KEY, path, 0)?;
    Ok(obj.into())
}

pub(super) fn get_llama_model(ctx: &mut ScopedContext, val: &JSValue) -> Result<LlamaModel, i32> {
    let opaque = val
        .retrive_object_payload(ctx, JSLlamaModel::class_id())?
        .other()
        .unwrap()
        .opaque();

    if !opaque.is_null() {
        Ok(unsafe { (&*opaque.cast::<LlamaModel>()).clone() })
    } else {
        ctx.throw_type_error("invalid Model object");
        Err(-1)
    }
}

const PROP_BOS_TOKEN: i32 = 1;
const PROP_PATH: i32 = 2;
const PROP_PARAMETERS: i32 = 3;
const PROP_VOCAB_SIZE: i32 = 4;
const PROP_BOS_TEXT: i32 = 5;
const PROP_EOS_TOKEN: i32 = 6;
const PROP_EOS_TEXT: i32 = 7;
const PROP_EOT_TOKEN: i32 = 8;
const PROP_EOT_TEXT: i32 = 9;
const PROP_SEP_TOKEN: i32 = 10;
const PROP_SEP_TEXT: i32 = 11;

fn js_mode_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    match cc.magic() {
        PROP_BOS_TOKEN => Ok(JSValue::Int(model.bos_token_id())),
        PROP_PATH => this.get_property_str(ctx, MODEL_PATH_HIDDEN_KEY),
        PROP_PARAMETERS => Ok((model.n_params() as f64).into()),
        PROP_VOCAB_SIZE => Ok(model.vocab_n_tokens().into()),
        PROP_EOS_TOKEN => Ok(JSValue::Int(model.eos_token_id())),
        PROP_BOS_TEXT => {
            if let Some(text) = model.bos_token_text() {
                ctx.create_string_from_utf8(text.into_bytes())
                    .map(JSValue::String)
            } else {
                Ok(JSValue::NULL)
            }
        }
        PROP_EOS_TEXT => {
            if let Some(text) = model.eos_token_text() {
                ctx.create_string_from_utf8(text.into_bytes())
                    .map(JSValue::String)
            } else {
                Ok(JSValue::NULL)
            }
        }
        PROP_EOT_TOKEN => Ok(JSValue::Int(model.eot_token_id())),
        PROP_EOT_TEXT => {
            if let Some(text) = model.eot_token_text() {
                ctx.create_string_from_utf8(text.into_bytes())
                    .map(JSValue::String)
            } else {
                Ok(JSValue::NULL)
            }
        }
        PROP_SEP_TOKEN => Ok(JSValue::Int(model.sep_token_id())),
        PROP_SEP_TEXT => {
            if let Some(text) = model.sep_token_text() {
                ctx.create_string_from_utf8(text.into_bytes())
                    .map(JSValue::String)
            } else {
                Ok(JSValue::NULL)
            }
        }
        _ => {
            unreachable!()
        }
    }
}

fn js_model_isEog(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let token = cc.arg(0).to_number(ctx)?.to_i32();
    Ok(JSValue::Bool(model.is_eog(token)))
}

pub(super) fn default_context_params() -> llama_cpp_ffi::llama_context_params {
    let mut ctx_params = LlamaContext::default_params();
    ctx_params.kv_unified = true;
    ctx_params.n_ctx = 4096;
    ctx_params.n_batch = 512;
    ctx_params.n_ubatch = 512;
    ctx_params.n_seq_max = 1;
    ctx_params.n_threads = 4;
    ctx_params.n_threads_batch = 8;
    ctx_params
}

pub(super) fn parse_context_params(
    ctx: &mut ScopedContext,
    opts: Option<&Object>,
) -> Result<llama_cpp_ffi::llama_context_params, i32> {
    let mut ctx_params = default_context_params();

    if let Some(opts) = opts {
        // --- Context & Sizing ---
        let v = opts.get_property_str(ctx, "contextSize")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 {
                ctx.throw_type_error("contextSize must be greater than 0");
                return Err(-1);
            }
            ctx_params.n_ctx = n as _;
        }

        let v = opts.get_property_str(ctx, "batchSize")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 {
                ctx.throw_type_error("batchSize must be greater than 0");
                return Err(-1);
            }
            ctx_params.n_batch = n as _;
        }

        let v = opts.get_property_str(ctx, "ubatchSize")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 {
                ctx.throw_type_error("ubatchSize must be greater than 0");
                return Err(-1);
            }
            ctx_params.n_ubatch = n as _;
        }

        let v = opts.get_property_str(ctx, "sequences")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 {
                ctx.throw_type_error("sequences must be greater than 0");
                return Err(-1);
            }
            ctx_params.n_seq_max = n as _;
        }

        // --- Threading ---
        let v = opts.get_property_str(ctx, "threads")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 {
                ctx.throw_type_error("threads must be greater than 0");
                return Err(-1);
            }
            ctx_params.n_threads = n;
        }

        let v = opts.get_property_str(ctx, "threadsBatch")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 {
                ctx.throw_type_error("threadsBatch must be greater than 0");
                return Err(-1);
            }
            ctx_params.n_threads_batch = n;
        }

        // --- Attention ---
        let v = opts.get_property_str(ctx, "attentionType")?;
        if !v.is_undefined() {
            let t = if let Some(s) = v.as_string() {
                s.content()
                    .encode_utf8(false)
                    .as_ref()
                    .parse::<AttentionType>()
                    .ok()
            } else {
                AttentionType::from_i32(v.to_number(ctx)?.to_i32())
            }
            .ok_or_else(|| {
                ctx.throw_type_error(
                    "attentionType must be one of: unspecified | causal | non-causal",
                );
                -1
            })?;
            ctx_params.attention_type = t as _;
        }

        let v = opts.get_property_str(ctx, "flashAttention")?;
        if !v.is_undefined() {
            let t = if let Some(s) = v.as_string() {
                s.content()
                    .encode_utf8(false)
                    .as_ref()
                    .parse::<FlashAttentionType>()
                    .ok()
            } else {
                FlashAttentionType::from_i32(v.to_number(ctx)?.to_i32())
            }
            .ok_or_else(|| {
                ctx.throw_type_error("flashAttention must be one of: auto | disabled | enabled");
                -1
            })?;
            ctx_params.flash_attn_type = t as _;
        }

        let v = opts.get_property_str(ctx, "swaFull")?;
        if !v.is_undefined() {
            ctx_params.swa_full = v.to_bool()?;
        }

        // --- Pooling & Embeddings ---
        let v = opts.get_property_str(ctx, "poolingType")?;
        if !v.is_undefined() {
            let t = if let Some(s) = v.as_string() {
                s.content()
                    .encode_utf8(false)
                    .as_ref()
                    .parse::<PoolingType>()
                    .ok()
            } else {
                PoolingType::from_i32(v.to_number(ctx)?.to_i32())
            }
            .ok_or_else(|| {
                ctx.throw_type_error(
                    "poolingType must be one of: unspecified | none | mean | cls | last | rank",
                );
                -1
            })?;
            ctx_params.pooling_type = t as _;
        }

        let v = opts.get_property_str(ctx, "embeddings")?;
        if !v.is_undefined() {
            ctx_params.embeddings = v.to_bool()?;
        }

        // --- RoPE & YaRN (nested under "rope") ---
        let rope_val = opts.get_property_str(ctx, "rope")?;
        if !rope_val.is_undefined() {
            let rope = rope_val.as_object().ok_or_else(|| {
                ctx.throw_type_error("rope should be an object");
                -1
            })?;

            let v = rope.get_property_str(ctx, "scalingType")?;
            if !v.is_undefined() {
                let t = if let Some(s) = v.as_string() {
                    s.content()
                        .encode_utf8(false)
                        .as_ref()
                        .parse::<RopeScalingType>()
                        .ok()
                } else {
                    RopeScalingType::from_i32(v.to_number(ctx)?.to_i32())
                }
                .ok_or_else(|| {
                    ctx.throw_type_error(
                        "rope.scalingType must be one of: unspecified | none | linear | yarn | longrope",
                    );
                    -1
                })?;
                ctx_params.rope_scaling_type = t as _;
            }

            let v = rope.get_property_str(ctx, "freqBase")?;
            if !v.is_undefined() {
                ctx_params.rope_freq_base = v.to_number(ctx)?.to_f64() as f32;
            }

            let v = rope.get_property_str(ctx, "freqScale")?;
            if !v.is_undefined() {
                ctx_params.rope_freq_scale = v.to_number(ctx)?.to_f64() as f32;
            }

            let v = rope.get_property_str(ctx, "yarnExtFactor")?;
            if !v.is_undefined() {
                ctx_params.yarn_ext_factor = v.to_number(ctx)?.to_f64() as f32;
            }

            let v = rope.get_property_str(ctx, "yarnAttnFactor")?;
            if !v.is_undefined() {
                ctx_params.yarn_attn_factor = v.to_number(ctx)?.to_f64() as f32;
            }

            let v = rope.get_property_str(ctx, "yarnBetaFast")?;
            if !v.is_undefined() {
                ctx_params.yarn_beta_fast = v.to_number(ctx)?.to_f64() as f32;
            }

            let v = rope.get_property_str(ctx, "yarnBetaSlow")?;
            if !v.is_undefined() {
                ctx_params.yarn_beta_slow = v.to_number(ctx)?.to_f64() as f32;
            }

            let v = rope.get_property_str(ctx, "yarnOrigContext")?;
            if !v.is_undefined() {
                let n = v.to_number(ctx)?.to_i32();
                if n < 0 {
                    ctx.throw_type_error("rope.yarnOrigContext must be >= 0");
                    return Err(-1);
                }
                ctx_params.yarn_orig_ctx = n as _;
            }
        }

        // --- KV Cache (nested under "cache") ---
        let cache_val = opts.get_property_str(ctx, "cache")?;
        if !cache_val.is_undefined() {
            let cache = cache_val.as_object().ok_or_else(|| {
                ctx.throw_type_error("cache should be an object");
                -1
            })?;

            let v = cache.get_property_str(ctx, "k")?;
            if !v.is_undefined() {
                let s = v.to_string(ctx)?;
                let t = s
                    .content()
                    .encode_utf8(false)
                    .parse::<KVCacheType>()
                    .map_err(|e| {
                        ctx.throw_type_error(&e);
                        -1
                    })?;
                ctx_params.type_k = t as _;
            }

            let v = cache.get_property_str(ctx, "v")?;
            if !v.is_undefined() {
                let s = v.to_string(ctx)?;
                let t = s
                    .content()
                    .encode_utf8(false)
                    .parse::<KVCacheType>()
                    .map_err(|e| {
                        ctx.throw_type_error(&e);
                        -1
                    })?;
                ctx_params.type_v = t as _;
            }
        }

        let v = opts.get_property_str(ctx, "defragThreshold")?;
        if !v.is_undefined() {
            ctx_params.defrag_thold = v.to_number(ctx)?.to_f64() as f32;
        }

        let v = opts.get_property_str(ctx, "kvUnified")?;
        if !v.is_undefined() {
            ctx_params.kv_unified = v.to_bool()?;
        }

        // --- Offloading & Performance ---
        let v = opts.get_property_str(ctx, "offloadKqv")?;
        if !v.is_undefined() {
            ctx_params.offload_kqv = v.to_bool()?;
        }

        let v = opts.get_property_str(ctx, "opOffload")?;
        if !v.is_undefined() {
            ctx_params.op_offload = v.to_bool()?;
        }

        let v = opts.get_property_str(ctx, "noPerf")?;
        if !v.is_undefined() {
            ctx_params.no_perf = v.to_bool()?;
        }
    }

    Ok(ctx_params)
}

fn js_model_createContext(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    let opts = if cc.length() > 0 {
        let v = cc.arg(0);
        if v.is_undefined() || v.is_null() {
            None
        } else {
            Some(v.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("params must be object");
                -1
            })?)
        }
    } else {
        None
    };
    let ctx_params = parse_context_params(ctx, opts.as_ref())?;

    let mut llm_ctx = LlamaContext::new(model, ctx_params).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    llm_ctx.init_batch(ctx_params.n_batch as i32, 0, ctx_params.n_seq_max as i32);

    let mut ctx_obj = JSLlamaContext::new_instance(ctx, llm_ctx)?;

    // to keep model object referenced alive, model property is read-only
    ctx_obj.define_property_value_str(ctx, "model", this, JS_PROP_ENUMERABLE)?;

    Ok(ctx_obj.into())
}

fn js_model_tokenize(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let llm_model = get_llama_model(ctx, &this)?;
    let text = cc.arg(0).to_string(ctx)?;

    let tokens = llm_model.tokenize(
        CString::new(text.content().encode_utf8(false).as_ref())
            .map_err(|_| {
                ctx.throw_internal_error("invalid text");
                -1
            })?
            .as_c_str(),
        false,
        false,
    );

    let res = TypedArray::create_from_vec_i32(ctx, tokens)?;
    Ok(res.into())
}

fn js_model_detokenize(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?;

    let mut output = String::new();
    let mut pending_bytes = Vec::new();

    for &token in tokens.iter() {
        let piece = model.token_to_piece(token);
        if piece.is_empty() {
            continue;
        }

        pending_bytes.extend_from_slice(&piece);

        loop {
            match std::str::from_utf8(&pending_bytes) {
                Ok(s) => {
                    output.push_str(s);
                    pending_bytes.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid == 0 {
                        break;
                    }
                    let s = unsafe { std::str::from_utf8_unchecked(&pending_bytes[..valid]) };
                    output.push_str(s);
                    pending_bytes.drain(..valid);
                    if pending_bytes.is_empty() {
                        break;
                    }
                }
            }
        }
    }

    ctx.create_string_from_utf8(output.into_bytes())
        .map(JSValue::String)
}

fn js_model_getMeta(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let key = cc.arg(0).to_string(ctx)?;
    let key = CString::new(key.content().encode_utf8(false).as_ref()).map_err(|_| {
        ctx.throw_type_error("invalid meta key");
        -1
    })?;

    if let Some(val) = model.meta_val_str(key.as_c_str()) {
        ctx.create_string_from_utf8(val.into_bytes())
            .map(JSValue::String)
    } else {
        Ok(JSValue::NULL)
    }
}

fn js_model_getMetaKeys(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let count = model.meta_count();

    let mut lst = Vec::<JSValue>::with_capacity(count as usize);

    for i in 0..count {
        if let Some(key) = model.meta_key_by_index(i) {
            let key_val = ctx
                .create_string_from_utf8(key.into_bytes())
                .map(JSValue::String)?;
            lst.push(key_val);
        }
    }

    Array::from_values(ctx, lst).map(JSValue::from)
}

fn js_model_lookupTokenText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let token = cc.arg(0).to_number(ctx)?.to_i32();

    if let Some(text) = model.lookup_token_text(token) {
        ctx.create_string_from_str(text).map(JSValue::String)
    } else {
        Ok(JSValue::NULL)
    }
}

#[derive(Debug)]
struct WellKnownTemplateVars {
    tools: JsonValue,
    add_generation_prompt: bool,
    enable_thinking: bool,
    add_vision_id: bool,
    eos_token: String,
    bos_token: String,
}

impl WellKnownTemplateVars {
    fn from_opts_map(
        ctx: &mut ScopedContext,
        model: &LlamaModel,
        opts_map: &mut serde_json::Map<String, JsonValue>,
    ) -> Result<Self, i32> {
        fn pop_bool(
            ctx: &mut ScopedContext,
            m: &mut serde_json::Map<String, JsonValue>,
            key: &str,
            default: bool,
        ) -> Result<bool, i32> {
            match m.remove(key) {
                Some(JsonValue::Bool(b)) => Ok(b),
                Some(_) => {
                    ctx.throw_type_error(&format!("renderTemplate options.{key} must be boolean"));
                    Err(-1)
                }
                None => Ok(default),
            }
        }

        fn pop_string(
            ctx: &mut ScopedContext,
            m: &mut serde_json::Map<String, JsonValue>,
            key: &str,
            default: String,
        ) -> Result<String, i32> {
            match m.remove(key) {
                Some(JsonValue::String(s)) => Ok(s),
                Some(_) => {
                    ctx.throw_type_error(&format!("renderTemplate options.{key} must be string"));
                    Err(-1)
                }
                None => Ok(default),
            }
        }

        let tools = opts_map.remove("tools").unwrap_or(JsonValue::Null);

        Ok(Self {
            tools,
            add_generation_prompt: pop_bool(ctx, opts_map, "add_generation_prompt", true)?,
            enable_thinking: pop_bool(ctx, opts_map, "enable_thinking", false)?,
            add_vision_id: pop_bool(ctx, opts_map, "add_vision_id", false)?,
            eos_token: pop_string(
                ctx,
                opts_map,
                "eos_token",
                model.eos_token_text().unwrap_or_default(),
            )?,
            bos_token: pop_string(
                ctx,
                opts_map,
                "bos_token",
                model.bos_token_text().unwrap_or_default(),
            )?,
        })
    }
}

fn get_preprocessed_template(
    ctx: &mut ScopedContext,
    this: &JSValue,
    model: &LlamaModel,
) -> Result<String, i32> {
    let cached = this.get_property_str(ctx, MODEL_CHAT_TEMPLATE_PREPROCESSED_KEY)?;
    if let Some(s) = cached.as_string() {
        return Ok(s.content().encode_utf8(false).to_string());
    }

    let key = CString::new("tokenizer.chat_template").unwrap();
    let raw = model.meta_val_str(key.as_c_str()).ok_or_else(|| {
        ctx.throw_internal_error("model meta tokenizer.chat_template not found");
        -1
    })?;
    let processed = preprocess_chat_template(&raw);

    let mut this_obj = this.as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("invalid Model object");
        -1
    })?;
    let v = ctx
        .create_string_from_utf8(processed.clone().into_bytes())
        .map(JSValue::String)?;
    this_obj.define_property_value_str(ctx, MODEL_CHAT_TEMPLATE_PREPROCESSED_KEY, v, 0)?;

    Ok(processed)
}

fn js_model_renderTemplate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    let msgs_arr = cc.arg(0).as_object_Array().copied().ok_or_else(|| {
        ctx.throw_type_error("messages must be array");
        -1
    })?;

    let mut messages: Vec<JinjaMessage> = Vec::with_capacity(msgs_arr.payload().len());
    for item in msgs_arr.payload().values() {
        let obj = item.as_object().copied().ok_or_else(|| {
            ctx.throw_type_error("messages[] item must be object");
            -1
        })?;

        let role_v = obj.get_property_str(ctx, "role")?;
        let role = role_v
            .as_string()
            .map(|x| x.content().encode_utf8(false).to_string())
            .ok_or_else(|| {
                ctx.throw_type_error("messages[].role must be string");
                -1
            })?;

        let content_v = obj.get_property_str(ctx, "content")?;
        let content = if content_v.is_undefined() || content_v.is_null() {
            String::new()
        } else if let Some(s) = content_v.as_string() {
            s.content().encode_utf8(false).to_string()
        } else {
            content_v
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string()
        };

        messages.push(JinjaMessage { role, content });
    }

    let mut well_known = WellKnownTemplateVars {
        tools: JsonValue::Null,
        add_generation_prompt: true,
        enable_thinking: false,
        add_vision_id: false,
        eos_token: model.eos_token_text().unwrap_or_default(),
        bos_token: model.bos_token_text().unwrap_or_default(),
    };
    let mut extra_vars = serde_json::Map::<String, JsonValue>::new();

    if cc.length() > 1 {
        let opts = cc.arg(1);
        if !opts.is_undefined() && !opts.is_null() {
            let opts = opts.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("options must be object");
                -1
            })?;

            let opts_json = ctx.with_new_scope(|scope| {
                scope.json_stringify(opts.into(), JSValue::UNDEFINED, JSValue::UNDEFINED)
            })?;
            if let Some(opts_json_str) = opts_json.as_string() {
                let s = opts_json_str.content().encode_utf8(false).to_string();
                if let Ok(JsonValue::Object(mut map)) = serde_json::from_str::<JsonValue>(&s) {
                    well_known = WellKnownTemplateVars::from_opts_map(ctx, &model, &mut map)?;
                    for (k, v) in map {
                        extra_vars.insert(k, v);
                    }
                }
            }
        }
    }

    let processed_template = get_preprocessed_template(ctx, &this, &model)?;

    let mut env = create_minijinja_env();
    env.add_template("chat", &processed_template).map_err(|e| {
        ctx.throw_internal_error(&format!("invalid chat template: {e}"));
        -1
    })?;
    let tmpl = env.get_template("chat").map_err(|e| {
        ctx.throw_internal_error(&format!("get template failed: {e}"));
        -1
    })?;

    let mut render_vars = serde_json::Map::<String, JsonValue>::new();
    render_vars.insert(
        "messages".to_string(),
        serde_json::to_value(&messages).map_err(|e| {
            ctx.throw_internal_error(&format!("serialize messages failed: {e}"));
            -1
        })?,
    );
    render_vars.insert("tools".to_string(), well_known.tools);
    render_vars.insert(
        "add_generation_prompt".to_string(),
        JsonValue::Bool(well_known.add_generation_prompt),
    );
    render_vars.insert(
        "enable_thinking".to_string(),
        JsonValue::Bool(well_known.enable_thinking),
    );
    render_vars.insert(
        "add_vision_id".to_string(),
        JsonValue::Bool(well_known.add_vision_id),
    );
    render_vars.insert(
        "eos_token".to_string(),
        JsonValue::String(well_known.eos_token),
    );
    render_vars.insert(
        "bos_token".to_string(),
        JsonValue::String(well_known.bos_token),
    );
    for (k, v) in extra_vars {
        render_vars.insert(k, v);
    }

    let out = tmpl.render(&render_vars).map_err(|e| {
        ctx.throw_internal_error(&format!("render chat template failed: {e}"));
        -1
    })?;

    ctx.create_string_from_utf8(out.into_bytes())
        .map(JSValue::String)
}

fn js_model_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaModel::class_id())?
        .other_mut()
        .unwrap();

    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Box::from_raw(p.cast::<LlamaModel>()));
        }
    }

    Ok(JSValue::UNDEFINED)
}

const CLASS_MODEL_PROTO_ENTRIES: &[FieldDef] = &[
    FieldDef::GETSET("bosToken", Some(js_mode_getter), None, PROP_BOS_TOKEN as _),
    FieldDef::GETSET("path", Some(js_mode_getter), None, PROP_PATH as _),
    FieldDef::GETSET(
        "parameters",
        Some(js_mode_getter),
        None,
        PROP_PARAMETERS as _,
    ),
    FieldDef::GETSET(
        "vocabSize",
        Some(js_mode_getter),
        None,
        PROP_VOCAB_SIZE as _,
    ),
    FieldDef::GETSET("bosText", Some(js_mode_getter), None, PROP_BOS_TEXT as _),
    FieldDef::GETSET("eosToken", Some(js_mode_getter), None, PROP_EOS_TOKEN as _),
    FieldDef::GETSET("eosText", Some(js_mode_getter), None, PROP_EOS_TEXT as _),
    FieldDef::GETSET("eotToken", Some(js_mode_getter), None, PROP_EOT_TOKEN as _),
    FieldDef::GETSET("eotText", Some(js_mode_getter), None, PROP_EOT_TEXT as _),
    FieldDef::GETSET("sepToken", Some(js_mode_getter), None, PROP_SEP_TOKEN as _),
    FieldDef::GETSET("sepText", Some(js_mode_getter), None, PROP_SEP_TEXT as _),
    FieldDef::CALL("createContext", 1, js_model_createContext, 0),
    FieldDef::CALL("createSamplerChain", 1, js_model_createSamplerChain, 0),
    FieldDef::CALL("tokenize", 1, js_model_tokenize, 0),
    FieldDef::CALL("detokenize", 1, js_model_detokenize, 0),
    FieldDef::CALL("lookupTokenText", 1, js_model_lookupTokenText, 0),
    FieldDef::CALL("isEog", 1, js_model_isEog, 0),
    FieldDef::CALL("getMeta", 1, js_model_getMeta, 0),
    FieldDef::CALL("getMetaKeys", 0, js_model_getMetaKeys, 0),
    FieldDef::CALL("renderTemplate", 2, js_model_renderTemplate, 0),
    FieldDef::CALL("dispose", 0, js_model_dispose, 0),
];

fn js_model_load(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let file_name = v.content().encode_utf8(false).to_string();

    let model_opts = parse_model_options(ctx, &cc)?;

    let (promise, tx) = Promise::create_with_channel(
        ctx,
        |cx, payload: Result<(LlamaModel, String), String>| -> JSResult {
            let (model, file_name) = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            let mut obj = JSLlamaModel::new_instance(cx, model)?;
            let path = cx
                .create_string_from_utf8(file_name.into_bytes())
                .map(JSValue::String)?;
            obj.define_property_value_str(cx, MODEL_PATH_HIDDEN_KEY, path, 0)?;
            Ok(JSValue::Object(obj))
        },
    )?;

    let _ = std::thread::spawn(move || {
        let mut model_params = LlamaModel::default_params();
        model_opts.apply_to_params(&mut model_params);
        normalize_model_params_for_runtime(&mut model_params);
        clear_llama_log_buffer();
        let r = LlamaModel::load_from_file(&CString::new(file_name.clone()).unwrap(), model_params)
            .map(|model| (model, file_name))
            .map_err(format_model_load_error);
        let _ = tx.send(r);
    });

    Ok(promise.into())
}

const CLASS_MODEL_ENTRIES: &[FieldDef] = &[FieldDef::CALL("load", 1, js_model_load, 0)];

pub struct JSLlamaModel;

impl JSLlamaModel {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    /// register class
    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        unsafe {
            extern "C" fn log_callback(
                level: llama_cpp_ffi::ggml_log_level,
                text: *const std::os::raw::c_char,
                _user_data: *mut std::os::raw::c_void,
            ) {
                if text.is_null() {
                    return;
                }

                if matches!(
                    level,
                    llama_cpp_ffi::ggml_log_level_GGML_LOG_LEVEL_WARN
                        | llama_cpp_ffi::ggml_log_level_GGML_LOG_LEVEL_ERROR
                ) {
                    let msg = unsafe { CStr::from_ptr(text).to_string_lossy() };
                    eprint!("{msg}");
                    append_llama_log(&msg);
                }
            }

            llama_cpp_ffi::llama_log_set(Some(log_callback), std::ptr::null_mut());
            llama_cpp_ffi::llama_backend_init();
        }

        let class_id = rt.register_class(
            ClassDef::new(
                "Model",
                CLASS_MODEL_PROTO_ENTRIES,
                Some(
                    ClassCtorDef::new(js_model_constructor, 2, false).entries(CLASS_MODEL_ENTRIES),
                ),
            )
            .on_dispose(|_, opaque: *mut c_void, _| {
                if !opaque.is_null() {
                    drop(unsafe { Box::from_raw(opaque.cast::<LlamaModel>()) });
                }
            })
            .on_unregister(|| unsafe {
                llama_cpp_ffi::llama_backend_free();
            }),
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

    pub fn new_instance(ctx: &mut ScopedContext, model: LlamaModel) -> Result<Object, i32> {
        let class_id = Self::class_id();
        let opaque = Box::into_raw(Box::new(model)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(class_id, opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<LlamaModel>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_model(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaModel::register(rt)
}
