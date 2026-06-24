use std::{
    cell::Cell,
    ffi::{CStr, CString},
    sync::{Arc, Mutex, OnceLock},
};

use linkme::distributed_slice;
use llama_cpp_ffi::{
    AttentionType, BackendDevice, FlashAttentionType, KVCacheType, LlamaContext, LlamaModel,
    ModelOptions, PoolingType, RopeScalingType, SplitMode,
};
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue,
    Object, Runtime, RuntimeImpl, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
    def_native_object,
};

use crate::{
    USER_CLASS_DEFS,
    helpers::{JinjaMessage, TemplateContext, create_minijinja_env},
};

use super::{
    class_context::{JSLlmContext, parse_tokens},
    class_sampler::js_model_createSamplerChain,
};

const MODEL_PATH_HIDDEN_KEY: &str = "__modelPath";
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
            // ensure the cut position is on a UTF-8 char boundary
            let boundary = s.floor_char_boundary(keep_from);
            s.drain(..boundary);
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
        log::warn!(
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
    let new_target = cc.this();

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

    let mut obj = JSLlmModel::new_instance(ctx, new_target, model)?;

    let path = ctx
        .create_string_from_utf8(file_name.into_bytes())
        .map(JSValue::String)?;
    obj.define_property_value_str(ctx, MODEL_PATH_HIDDEN_KEY, path, 0)?;

    Ok(obj.into())
}

pub(super) fn get_llama_model<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a LlamaModel, i32> {
    val.as_native_object::<JSLlmModel>()
        .and_then(|ob| ob.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("invalid Model object");
            -1
        })

    // JSLlamaModel::try_from(val)
    //     .unwrap()
    //     .payload()
    //     .cloned()
    //     .ok_or_else(|| {
    //         ctx.throw_type_error("invalid Model object");
    //         -1
    //     })
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
const PROP_DESC: i32 = 12;
const PROP_CHAT_TEMPLATE: i32 = 13;

fn js_mode_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;

    match cc.magic() {
        PROP_DESC => ctx
            .create_string_from_utf8(model.model_desc().into_bytes())
            .map(JSValue::from),
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
        PROP_CHAT_TEMPLATE => {
            if let Some(text) = model.with_chat_template(|t| ctx.create_string_from_str(t)) {
                text.map(JSValue::String)
            } else {
                Ok(JSValue::NULL)
            }
        }
        _ => {
            unreachable!()
        }
    }
}

fn js_mode_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let model = get_llama_model(ctx, &this)?;
    let val = cc.arg(0);

    if cc.magic() == PROP_CHAT_TEMPLATE {
        let t = val.to_string(ctx)?.content().encode_utf8(false).to_string();
        model.set_chat_template(t);
    }

    Ok(JSValue::UNDEFINED)
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

    let mut llm_ctx = LlamaContext::new(model.clone(), ctx_params).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    llm_ctx.init_batch(ctx_params.n_batch as i32, 0, ctx_params.n_seq_max as i32);

    let mut ctx_obj =
        JSLlmContext::new_instance(ctx, JSValue::UNDEFINED, Arc::new(Mutex::new(Some(llm_ctx))))?;

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

    // Parse options into TemplateContext
    let mut tmpl_ctx = TemplateContext::new(
        &messages,
        model.eos_token_text().unwrap_or_default(),
        model.bos_token_text().unwrap_or_default(),
    );
    let mut extra_vars = serde_json::Map::<String, serde_json::Value>::new();

    let val = cc.arg(1);
    if !val.is_undefined() {
        let opts = val.as_object().copied().ok_or_else(|| {
            ctx.throw_type_error("options must be object");
            -1
        })?;

        let json = ctx.with_new_scope(|scope| {
            scope.json_stringify(opts.into(), JSValue::UNDEFINED, JSValue::UNDEFINED)
        })?;

        if let Some(json_str) = json.as_string()
            && let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(
                json_str.content().encode_utf8(false).as_ref(),
            )
        {
            // Extract well-known template variables
            fn pop_bool(
                ctx: &mut ScopedContext,
                m: &mut serde_json::Map<String, serde_json::Value>,
                key: &str,
                default: bool,
            ) -> Result<bool, i32> {
                match m.remove(key) {
                    Some(serde_json::Value::Bool(b)) => Ok(b),
                    Some(_) => {
                        ctx.throw_type_error(&format!(
                            "renderTemplate options.{key} must be boolean"
                        ));
                        Err(-1)
                    }
                    None => Ok(default),
                }
            }

            fn pop_string(
                ctx: &mut ScopedContext,
                m: &mut serde_json::Map<String, serde_json::Value>,
                key: &str,
                default: String,
            ) -> Result<String, i32> {
                match m.remove(key) {
                    Some(serde_json::Value::String(s)) => Ok(s),
                    Some(_) => {
                        ctx.throw_type_error(&format!(
                            "renderTemplate options.{key} must be string"
                        ));
                        Err(-1)
                    }
                    None => Ok(default),
                }
            }

            tmpl_ctx.tools = map.remove("tools");
            tmpl_ctx.add_generation_prompt =
                pop_bool(ctx, &mut map, "add_generation_prompt", true)?;
            tmpl_ctx.enable_thinking = pop_bool(ctx, &mut map, "enable_thinking", false)?;
            tmpl_ctx.add_vision_id = pop_bool(ctx, &mut map, "add_vision_id", false)?;
            tmpl_ctx.eos_token = pop_string(
                ctx,
                &mut map,
                "eos_token",
                model.eos_token_text().unwrap_or_default(),
            )?;
            tmpl_ctx.bos_token = pop_string(
                ctx,
                &mut map,
                "bos_token",
                model.bos_token_text().unwrap_or_default(),
            )?;

            // Remaining keys go to extra_vars
            for (k, v) in map {
                extra_vars.insert(k, v);
            }
        }
    }

    let rendered = model
        .with_chat_template(|tmpl| -> Result<String, minijinja::Error> {
            let mut env = create_minijinja_env();

            // parse template
            env.add_template("chat", tmpl)?;
            // render template
            env.get_template("chat").and_then(|t| t.render(&tmpl_ctx))
        })
        .ok_or_else(|| {
            ctx.throw_internal_error("no chat template");
            -1
        })?
        .map_err(|e| {
            ctx.throw_internal_error(&format!("fail to apply chat template: {e}"));
            -1
        })?;

    ctx.create_string_from_utf8(rendered.into_bytes())
        .map(JSValue::String)
}

fn js_model_dispose(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let _ = JSLlmModel::try_from(cc.this()).unwrap().take_payload();
    Ok(JSValue::UNDEFINED)
}

const CLASS_MODEL_PROTO_ENTRIES: &[FieldDef] = &[
    FieldDef::GETSET("path", Some(js_mode_getter), None, PROP_PATH as _),
    FieldDef::GETSET("desc", Some(js_mode_getter), None, PROP_DESC as _),
    FieldDef::GETSET("bosToken", Some(js_mode_getter), None, PROP_BOS_TOKEN as _),
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
    FieldDef::GETSET(
        "chatTemplate",
        Some(js_mode_getter),
        Some(js_mode_setter),
        PROP_CHAT_TEMPLATE as _,
    ),
    FieldDef::FUNC("createContext", 1, js_model_createContext, 0),
    FieldDef::FUNC("createSamplerChain", 1, js_model_createSamplerChain, 0),
    FieldDef::FUNC("tokenize", 1, js_model_tokenize, 0),
    FieldDef::FUNC("detokenize", 1, js_model_detokenize, 0),
    FieldDef::FUNC("lookupTokenText", 1, js_model_lookupTokenText, 0),
    FieldDef::FUNC("isEog", 1, js_model_isEog, 0),
    FieldDef::FUNC("getMeta", 1, js_model_getMeta, 0),
    FieldDef::FUNC("getMetaKeys", 0, js_model_getMetaKeys, 0),
    FieldDef::FUNC("renderTemplate", 2, js_model_renderTemplate, 0),
    FieldDef::FUNC("dispose", 0, js_model_dispose, 0),
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
            let mut obj = JSLlmModel::new_instance(cx, JSValue::UNDEFINED, model)?;
            let path = cx
                .create_string_from_utf8(file_name.into_bytes())
                .map(JSValue::String)?;
            obj.define_property_value_str(cx, MODEL_PATH_HIDDEN_KEY, path, 0)?;
            Ok(obj.into())
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

const CLASS_MODEL_ENTRIES: &[FieldDef] = &[FieldDef::FUNC("load", 1, js_model_load, 0)];

def_native_object!(
    "Model",
    JSLlmModel,
    LlamaModel,
    CLASS_MODEL_PROTO_ENTRIES,
    Some(ClassCtorDef::new(js_model_constructor, 2, false).entries(CLASS_MODEL_ENTRIES)),
    on_class_register: JSLlmModel::on_register,
    on_class_unregister: JSLlmModel::on_unregister
);

impl JSLlmModel {
    fn on_register(_: &mut RuntimeImpl) {
        unsafe {
            extern "C" fn log_callback(
                level: llama_cpp_ffi::ggml_log_level,
                text: *const std::os::raw::c_char,
                _user_data: *mut std::os::raw::c_void,
            ) {
                if text.is_null() {
                    return;
                }

                let msg = unsafe { CStr::from_ptr(text).to_string_lossy() };

                match level {
                    llama_cpp_ffi::ggml_log_level_GGML_LOG_LEVEL_DEBUG => {
                        log::debug!("{msg}");
                    }
                    llama_cpp_ffi::ggml_log_level_GGML_LOG_LEVEL_INFO => {
                        log::info!("{msg}");
                    }
                    llama_cpp_ffi::ggml_log_level_GGML_LOG_LEVEL_WARN => {
                        log::warn!("{msg}");
                    }
                    llama_cpp_ffi::ggml_log_level_GGML_LOG_LEVEL_ERROR => {
                        log::error!("{msg}");
                    }
                    _ => {
                        log::info!("{msg}");
                    }
                }

                append_llama_log(&msg);
            }

            llama_cpp_ffi::llama_log_set(Some(log_callback), std::ptr::null_mut());
            llama_cpp_ffi::llama_backend_init();
        }
    }

    fn on_unregister(_: &mut RuntimeImpl) {
        unsafe {
            llama_cpp_ffi::llama_backend_free();
        }
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_model(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlmModel::register(rt)
}
