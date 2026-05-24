use std::{
    cell::Cell,
    ffi::CString,
    sync::{Arc, Mutex},
};

use async_openai::types::chat::{ReasoningEffort, StopConfiguration};
use linkme::distributed_slice;
use llama_cpp_ffi::ToolChoice as LarkToolChoice;
use zuqe::{
    CallContext, ClassID, FieldDef, JSResult, JSValue, NativeObject, Object, Runtime,
    ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
    def_native_object,
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_client_responses::{parse_responses_input, parse_responses_reasoning},
    engine::{Engine, GrammarConfig, TaskConfig},
    helpers::{
        PromptThinkPattern, apply_chat_template, detect_prompt_think_pattern,
        parse_chat_completion_req, to_lark_tool_choice,
    },
    output::{
        ChunkSSE, ParserKind, collect_engine_reply_to_json, redirect_engine_reply_to_chat_sse,
        redirect_engine_reply_to_responses_sse,
    },
};

#[repr(transparent)]
#[derive(Debug, Clone)]
pub(crate) struct EnginePayload(Arc<Mutex<Engine>>);

impl std::ops::Deref for EnginePayload {
    type Target = Arc<Mutex<Engine>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Arc<Mutex<Engine>>> for EnginePayload {
    fn from(engine: Arc<Mutex<Engine>>) -> Self {
        Self(engine)
    }
}

pub(crate) const ERR_CANCELLED: &str = "cancelled";
pub(crate) const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";

pub(super) struct EngineParams {
    pub idle_sleep_ms: u64,
    pub slot_match_threshold: f32,
    pub max_tokens: usize,
}

pub(super) fn parse_engine_params(
    ctx: &mut ScopedContext,
    opts: Option<&Object>,
) -> Result<EngineParams, i32> {
    let mut idle_sleep_ms = 1u64;
    let mut slot_match_threshold = 0.5f32;
    let mut max_tokens = 0usize;

    if let Some(opts) = opts {
        // idleSleepMs
        let v = opts.get_property_str(ctx, "idleSleepMs")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                idle_sleep_ms = n as u64;
            } else {
                ctx.throw_type_error("idleSleepMs must be >= 0");
                return Err(-1);
            }
        }

        // slotMatchThreshold
        let v = opts.get_property_str(ctx, "slotMatchThreshold")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..=1.0).contains(&d) {
                slot_match_threshold = d as f32;
            } else {
                ctx.throw_type_error("slotMatchThreshold must be in [0, 1]");
                return Err(-1);
            }
        }

        // maxCompletionTokens
        let v = opts.get_property_str(ctx, "maxCompletionTokens")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n < 0 {
                ctx.throw_type_error("maxCompletionTokens must be >= 0");
                return Err(-1);
            }
            max_tokens = n as usize;
        }
    }

    Ok(EngineParams {
        idle_sleep_ms,
        slot_match_threshold,
        max_tokens,
    })
}

fn get_engine<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<EnginePayload, i32> {
    let engine = JSEngine::try_from(val.clone()).unwrap();
    if let Some(p) = engine.payload() {
        Ok(p.clone())
    } else {
        ctx.throw_type_error("Engine was disposed");
        Err(-1)
    }
}

pub(super) const MAGIC_CHAT_ONCE: i32 = 2;
pub(super) const MAGIC_CHAT_STREAM: i32 = 3;
pub(super) const MAGIC_RESPONSES_ONCE: i32 = 5;
pub(super) const MAGIC_RESPONSES_STREAM: i32 = 4;

fn stop_to_vec(stop: &StopConfiguration) -> Option<Vec<String>> {
    match stop {
        StopConfiguration::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(vec![s.clone()])
            }
        }
        StopConfiguration::StringArray(arr) => {
            if arr.is_empty() {
                None
            } else {
                Some(arr.clone())
            }
        }
    }
}

fn js_engine_chatCompletions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    let opts_obj = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("options must be object");
        -1
    })?;

    let mut req = opts_obj.to_serde_json(ctx).and_then(|v| {
        parse_chat_completion_req(v).map_err(|_| {
            ctx.throw_type_error("failed to serialize options");
            -1
        })
    })?;

    let model_name: String;
    let parse_reasoning: bool;
    let output_prepend: Vec<i32>;

    let enable_thinking = req.reasoning_effort.clone().unwrap_or_default() != ReasoningEffort::None;

    // Build prompt tokens
    let prompt_tokens = {
        let engine = engine.lock().unwrap();
        let model = engine.model();

        model_name = engine.model_name().to_string();
        parse_reasoning = engine.parse_reasoning();

        let prompt = apply_chat_template(
            model,
            &req.messages,
            req.tools.as_deref(),
            req.tool_choice.as_ref(),
            req.parallel_tool_calls,
            req.reasoning_effort.clone().unwrap_or_default(),
            req.response_format.as_ref(),
        )
        .map_err(|e| {
            ctx.throw_internal_error(&e);
            -1
        })?;

        // check prompt ending <think> pattern
        output_prepend = if enable_thinking && parse_reasoning {
            let tags = model.arch().think_boundaries();
            if detect_prompt_think_pattern(&prompt, tags.0, tags.1) == PromptThinkPattern::Open {
                model.tokenize(CString::new(tags.0).unwrap().as_c_str(), false, false)
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let tokens = model.tokenize(&CString::new(prompt).unwrap(), false, true);
        if tokens.is_empty() {
            ctx.throw_internal_error("prompt tokenized to zero tokens");
            return Err(-1);
        }

        tokens
    };

    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;
    if req.model.is_empty() {
        req.model = engine.lock().unwrap().model_name().into();
    }

    // Build task config
    let mut task_config = TaskConfig::new()
        .temperature(req.temperature.unwrap_or(0.75))
        .top_p(req.top_p.unwrap_or(1.0))
        .max_tokens(
            req.max_completion_tokens
                .map_or(engine.lock().unwrap().default_max_tokens(), |n| n as usize),
        )
        .output_stream(is_stream)
        .enable_output_parser(parse_reasoning);

    if let Some(stop_words) = req
        .stop
        .as_ref()
        .and_then(stop_to_vec)
        .filter(|w| !w.is_empty())
    {
        task_config = task_config.stop_words(stop_words);
    }

    if let Some(tools) = req.tools.as_ref().filter(|t| !t.is_empty()) {
        let tc = req
            .tool_choice
            .as_ref()
            .map(to_lark_tool_choice)
            .unwrap_or(LarkToolChoice::Auto);

        if tc != LarkToolChoice::None {
            task_config = task_config.grammar(GrammarConfig::new(
                engine.lock().unwrap().model(),
                tools,
                tc,
            ));
        }
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());

    // Push task to engine
    let rx = engine
        .lock()
        .unwrap()
        .push_task(prompt_tokens, task_config, output_prepend, None, true)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    let include_usage = req
        .stream_options
        .is_some_and(|s| s.include_usage.unwrap_or(false));

    if is_stream {
        // Streaming: AsyncIterableIterator
        let (chunk_tx, chunk_rx) = crossbeam::channel::bounded::<crate::output::ChunkSSE>(8);
        std::thread::spawn(move || {
            redirect_engine_reply_to_chat_sse(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                n_prompt_tokens,
                include_usage,
                |chunk| chunk_tx.send(chunk).is_ok(),
            );
        });

        AsyncIterableIterator::create_with_receiver::<crate::output::ChunkSSE>(
            ctx,
            chunk_rx,
            move |cx, chunk| -> Result<Option<JSValue>, i32> {
                match chunk {
                    crate::output::ChunkSSE::Chunk(data) => {
                        JSValue::from_serde_json(cx, &data).map(Some).map_err(|_| {
                            cx.throw_internal_error("serialize chunk failed");
                            -1
                        })
                    }
                    crate::output::ChunkSSE::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                    crate::output::ChunkSSE::Done => {
                        // End the iterator
                        Ok(None)
                    }
                }
            },
        )
        .map(JSValue::from)
    } else {
        // Non-streaming: Promise
        let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
        let promise =
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, res| match res {
                Ok(val) => JSValue::from_serde_json(cx, &val),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            })?;

        std::thread::spawn(move || {
            let result = collect_engine_reply_to_json(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                ParserKind::Chat,
                &respond_id,
                &model_name,
                n_prompt_tokens,
                include_usage,
            );
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}

fn js_engine_responses(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_RESPONSES_STREAM;

    let opts = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("responses() requires an options object");
        -1
    })?;

    let input_val = opts.get_property_str(ctx, "input")?;
    let input = parse_responses_input(ctx, &input_val)?;

    let temperature_val = opts.get_property_str(ctx, "temperature")?;
    let temperature = if !temperature_val.is_undefined() {
        Some(temperature_val.to_number(ctx)?.to_f64() as f32)
    } else {
        None
    };

    let top_p_val = opts.get_property_str(ctx, "topP")?;
    let top_p = if !top_p_val.is_undefined() {
        Some(top_p_val.to_number(ctx)?.to_f64() as f32)
    } else {
        None
    };

    let max_output_tokens_val = opts.get_property_str(ctx, "maxOutputTokens")?;
    let max_output_tokens = if !max_output_tokens_val.is_undefined() {
        Some(max_output_tokens_val.to_number(ctx)?.to_i32() as usize)
    } else {
        None
    };

    let reasoning_val = opts.get_property_str(ctx, "reasoning")?;
    let reasoning = if !reasoning_val.is_undefined() {
        Some(parse_responses_reasoning(ctx, &reasoning_val)?)
    } else {
        None
    };

    let tools_val = opts.get_property_str(ctx, "tools")?;
    let tools = if !tools_val.is_undefined() {
        let t: Vec<async_openai::types::responses::Tool> =
            serde_json::from_value(tools_val.to_serde_json(ctx).map_err(|_| {
                ctx.throw_type_error("failed to serialize tools");
                -1
            })?)
            .map_err(|e| {
                ctx.throw_type_error(&format!("invalid tools: {e}"));
                -1
            })?;
        if t.is_empty() { None } else { Some(t) }
    } else {
        None
    };

    let usage_val = opts.get_property_str(ctx, "usage")?;
    let usage = if !usage_val.is_undefined() {
        usage_val.to_bool()?
    } else {
        false
    };

    // Convert responses input items to chat messages
    let input_items = match &input {
        async_openai::types::responses::InputParam::Text(t) => {
            vec![async_openai::types::responses::InputItem::EasyMessage(
                async_openai::types::responses::EasyInputMessage {
                    r#type: async_openai::types::responses::MessageType::Message,
                    role: async_openai::types::responses::Role::User,
                    content: async_openai::types::responses::EasyInputContent::Text(t.clone()),
                    phase: None,
                },
            )]
        }
        async_openai::types::responses::InputParam::Items(items) => items.clone(),
    };
    if input_items.is_empty() {
        ctx.throw_type_error("\"input\" must contain at least one valid message");
        return Err(-1);
    }
    let messages: Vec<_> = input_items
        .iter()
        .filter_map(crate::responses::convert::input_item_to_chat_message)
        .collect();

    // Build prompt tokens
    let chat_tools: Option<Vec<async_openai::types::chat::ChatCompletionTools>> = tools
        .as_ref()
        .map(|t| crate::responses::convert::responses_tools_to_chat_tools(t));

    let reasoning_effort = reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .unwrap_or_default();

    let enable_thinking = reasoning_effort != ReasoningEffort::None;
    let parse_reasoning: bool;
    let output_prepend: Vec<i32>;

    // Build prompt tokens
    let prompt_tokens = {
        let engine = engine.lock().unwrap();
        let model = engine.model();
        parse_reasoning = engine.parse_reasoning();

        let prompt = apply_chat_template(
            model,
            &messages,
            chat_tools.as_deref(),
            None,
            None,
            reasoning_effort,
            None,
        )
        .map_err(|e| {
            ctx.throw_internal_error(&e);
            -1
        })?;

        output_prepend = if enable_thinking && parse_reasoning {
            let tags = model.arch().think_boundaries();
            if detect_prompt_think_pattern(&prompt, tags.0, tags.1) == PromptThinkPattern::Open {
                model.tokenize(CString::new(tags.0).unwrap().as_c_str(), false, false)
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let tokens = model.tokenize(&CString::new(prompt).unwrap(), false, true);
        if tokens.is_empty() {
            ctx.throw_internal_error("prompt tokenized to zero tokens");
            return Err(-1);
        }

        tokens
    };

    // Build task config
    let mut task_config = TaskConfig::new()
        .temperature(temperature.unwrap_or(0.75))
        .top_p(top_p.unwrap_or(1.0))
        .max_tokens(max_output_tokens.unwrap_or(engine.lock().unwrap().default_max_tokens()))
        .output_stream(is_stream);

    if let Some(tools) = chat_tools.as_ref().filter(|t| !t.is_empty()) {
        task_config = task_config.grammar(GrammarConfig::new(
            engine.lock().unwrap().model(),
            tools,
            llama_cpp_ffi::ToolChoice::Auto,
        ));
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("res-{}", uuid::Uuid::new_v4().simple());
    let model_name = engine.lock().unwrap().model_name().to_string();

    // Push task to engine
    let rx = engine
        .lock()
        .unwrap()
        .push_task(prompt_tokens, task_config, output_prepend, None, false)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    if is_stream {
        // Streaming: AsyncIterableIterator
        let (item_tx, item_rx) = crossbeam::channel::bounded::<ChunkSSE>(8);
        std::thread::spawn(move || {
            redirect_engine_reply_to_responses_sse(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                n_prompt_tokens,
                false,
                |chunk| item_tx.send(chunk).is_ok(),
            );
        });

        AsyncIterableIterator::create_with_receiver::<ChunkSSE>(
            ctx,
            item_rx,
            move |cx, chunk| -> Result<Option<JSValue>, i32> {
                match chunk {
                    ChunkSSE::Done => Ok(None),
                    ChunkSSE::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                    ChunkSSE::Chunk(value) => JSValue::from_serde_json(cx, &value).map(Some),
                }
            },
        )
        .map(JSValue::from)
    } else {
        // Non-streaming: Promise
        let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
        let promise =
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, res| match res {
                Ok(val) => JSValue::from_serde_json(cx, &val),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            })?;

        std::thread::spawn(move || {
            let result = collect_engine_reply_to_json(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                ParserKind::Responses,
                &respond_id,
                &model_name,
                n_prompt_tokens,
                usage,
            );
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}

fn js_engine_httpServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8080;

    // Parse options
    let opts = if cc.length() > 0 {
        let v = cc.arg(0);
        if v.is_undefined() || v.is_null() {
            None
        } else {
            Some(v.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("options must be object");
                -1
            })?)
        }
    } else {
        None
    };
    if let Some(opts) = opts.as_ref() {
        let v = opts.get_property_str(ctx, "host")?;
        if !v.is_undefined() {
            let s = v.to_string(ctx)?;
            host = s.content().encode_utf8(false).to_string();
        }

        let v = opts.get_property_str(ctx, "port")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n <= 0 || n > 65535 {
                ctx.throw_type_error("port must be 1..65535");
                return Err(-1);
            }
            port = n as u16;
        }
    }

    if engine.lock().unwrap().is_serving() {
        ctx.throw_type_error("engine is already serving");
        return Err(-1);
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    engine.lock().unwrap().set_serving(shutdown_tx);

    // Start HTTP server
    let result_slot = PromiseResultSlot::<Result<(), String>>::new();

    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, res| match res {
            Ok(()) => Ok(JSValue::UNDEFINED),
            Err(e) => {
                cx.throw_internal_error(&e);
                Err(-1)
            }
        })?;

    tokio::spawn(async move {
        let res = crate::serve::http_serve(engine.0.clone(), &host, port, shutdown_rx).await;
        engine.lock().unwrap().unset_serving();
        let _ = result_slot.commit(res);
    });

    Ok(promise.into())
}

fn js_engine_stopServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    engine.lock().unwrap().request_stop();
    Ok(JSValue::UNDEFINED)
}

fn js_engine_isServing(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    Ok(JSValue::Bool(engine.lock().unwrap().is_serving()))
}

fn js_engine_parseReasoning(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    Ok(JSValue::Bool(engine.lock().unwrap().parse_reasoning()))
}

fn js_engine_setParseReasoning(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    let v = cc.arg(0).to_bool()?;
    engine.lock().unwrap().set_parse_reasoning(v);
    Ok(JSValue::UNDEFINED)
}

fn js_engine_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSEngine::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Box::from_raw(p.cast::<EnginePayload>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

const CLASS_ENGINE_PROTO: &[FieldDef] = &[
    FieldDef::FUNC(
        "chat",
        2,
        js_engine_chatCompletions,
        MAGIC_CHAT_STREAM as i16,
    ),
    FieldDef::FUNC(
        "chatOnce",
        2,
        js_engine_chatCompletions,
        MAGIC_CHAT_ONCE as i16,
    ),
    FieldDef::FUNC(
        "responses",
        1,
        js_engine_responses,
        MAGIC_RESPONSES_STREAM as i16,
    ),
    FieldDef::FUNC(
        "responsesOnce",
        1,
        js_engine_responses,
        MAGIC_RESPONSES_ONCE as i16,
    ),
    FieldDef::FUNC("httpServe", 1, js_engine_httpServe, 0),
    FieldDef::FUNC("stopServe", 0, js_engine_stopServe, 0),
    FieldDef::GETSET("isServing", Some(js_engine_isServing), None, 0),
    FieldDef::GETSET(
        "parseReasoning",
        Some(js_engine_parseReasoning),
        Some(js_engine_setParseReasoning),
        0,
    ),
    FieldDef::FUNC("dispose", 0, js_engine_dispose, 0),
];

def_native_object!("Engine", JSEngine, EnginePayload, CLASS_ENGINE_PROTO, None);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_engine(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSEngine::register(rt)
}
