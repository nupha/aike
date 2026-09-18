// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#![cfg(feature = "llama_cpp")]

use std::{
    ffi::CString,
    sync::{Arc, Mutex},
};

use async_openai::types::chat::{ReasoningEffort, StopConfiguration};
use llama::ToolChoice as LarkToolChoice;
use zuqe::{
    CallContext, FieldDef, JSResult, JSValue, Object, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    ai::output::{
        ChunkSSE, ParserKind, collect_engine_reply_to_json, redirect_engine_reply_to_chat_sse,
        redirect_engine_reply_to_responses_sse,
    },
    helpers::{
        PromptThinkPattern, apply_chat_template, compute_keep_tokens, detect_prompt_think_pattern,
        parse_chat_completion_req, to_lark_tool_choice,
    },
};

#[cfg(feature = "enable_eval_log")]
use crate::eval_logger::{EvalLogEntry, ToolCallLog, get_eval_logger};

use super::{
    MAGIC_CHAT_ONCE, MAGIC_CHAT_STREAM, MAGIC_RESPONSES_ONCE, MAGIC_RESPONSES_STREAM,
    class_client_response::{parse_responses_input, parse_responses_reasoning},
    engine::{Engine, GrammarConfig, TaskConfig},
};

pub(crate) const ERR_CANCELLED: &str = "cancelled";
pub(crate) const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";

pub(super) struct EngineParams {
    pub n_cache_reuse: usize,
    pub cache_ram_mb: usize,
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
    let mut n_cache_reuse = 0usize;
    let mut cache_ram_mb = 1024_usize; // default 1G limit for cache ram

    if let Some(opts) = opts {
        // idleSleepMs
        let v = opts.get_property_str(ctx, "idleSleepMs")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                idle_sleep_ms = n as u64;
            } else {
                return Err(ctx.throw_type_error("idleSleepMs must be >= 0"));
            }
        }

        // slotMatchThreshold
        let v = opts.get_property_str(ctx, "slotMatchThreshold")?;
        if !v.is_undefined() {
            let d = v.to_number(ctx)?.to_f64();
            if (0.0..=1.0).contains(&d) {
                slot_match_threshold = d as f32;
            } else {
                return Err(ctx.throw_type_error("slotMatchThreshold must be in [0, 1]"));
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

        // cacheReuse (KV shifting minimum chunk size, 0 = disabled)
        let v = opts.get_property_str(ctx, "cacheReuse")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                n_cache_reuse = n as usize;
            } else {
                ctx.throw_type_error("cacheReuse must be >= 0");
                return Err(-1);
            }
        }

        // cacheRam (MB)
        let v = opts.get_property_str(ctx, "cacheRam")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                cache_ram_mb = n as usize;
            } else {
                ctx.throw_type_error("cacheRam must be >= 0");
                return Err(-1);
            }
        }
    }

    Ok(EngineParams {
        idle_sleep_ms,
        slot_match_threshold,
        max_tokens,
        n_cache_reuse,
        cache_ram_mb,
    })
}

fn get_engine(ctx: &mut ScopedContext, val: &JSValue) -> Result<Arc<Mutex<Engine>>, i32> {
    let engine = JSEngine::try_from(*val).unwrap();
    if let Some(p) = engine.payload() {
        Ok(p.clone())
    } else {
        ctx.throw_type_error("Engine was detached");
        Err(-1)
    }
}

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

    let opts_obj = cc.arg(0).as_object().ok_or_else(|| {
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

    let keep_tokens = {
        let engine = engine.lock().unwrap();
        let model = engine.model();
        compute_keep_tokens(model, &req.messages)
    };

    // Build task config
    let mut task_config = TaskConfig::new()
        .temperature(req.temperature.unwrap_or(0.75))
        .top_p(req.top_p.unwrap_or(1.0))
        .max_tokens(
            req.max_completion_tokens
                .map_or(engine.lock().unwrap().default_max_tokens(), |n| n as usize),
        )
        .output_stream(is_stream)
        .enable_output_parser(parse_reasoning)
        .keep_tokens(keep_tokens);

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
        .push_task(prompt_tokens, task_config, output_prepend)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    let include_usage = req
        .stream_options
        .is_some_and(|s| s.include_usage.unwrap_or(false));

    // 提取输入用于评估日志（在 req 被 move 前）
    #[cfg(feature = "enable_eval_log")]
    let log_input = req
        .messages
        .iter()
        .filter_map(|m| {
            let (role, content) = crate::helpers::message_to_role_content(m);
            (role == "user").then_some(content)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let arch = engine.lock().unwrap().model().arch();

    if is_stream {
        // Streaming: AsyncIterableIterator
        let (chunk_tx, chunk_rx) = ctx.make_channel::<crate::ai::output::ChunkSSE>(8);
        std::thread::spawn(move || {
            redirect_engine_reply_to_chat_sse(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                n_prompt_tokens,
                include_usage,
                |chunk| chunk_tx.send(chunk).is_ok(),
                arch,
            );
        });

        AsyncIterableIterator::create_with_receiver::<crate::ai::output::ChunkSSE>(
            ctx,
            chunk_rx,
            move |cx, chunk| -> Result<Option<JSValue>, i32> {
                match chunk {
                    crate::ai::output::ChunkSSE::Chunk(data) => {
                        JSValue::from_serde_json(cx, &data).map(Some).map_err(|_| {
                            cx.throw_internal_error("serialize chunk failed");
                            -1
                        })
                    }
                    crate::ai::output::ChunkSSE::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                    crate::ai::output::ChunkSSE::Done => {
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
            #[cfg(feature = "enable_eval_log")]
            let start = std::time::Instant::now();

            let result = collect_engine_reply_to_json(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                ParserKind::Chat,
                &respond_id,
                &model_name,
                n_prompt_tokens,
                include_usage,
                arch,
            );

            #[cfg(feature = "enable_eval_log")]
            if let Ok(ref val) = result {
                let latency = start.elapsed().as_millis();
                let mut output = String::new();
                let mut tool_calls = Vec::new();
                if let Some(choices) = val
                    .get(crate::ai::OUTMSG_FIELD_CHOICES)
                    .and_then(|c| c.as_array())
                {
                    if let Some(choice) = choices.first() {
                        if let Some(message) = choice.get("message") {
                            if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
                                output = content.to_string();
                            }
                            if let Some(calls) =
                                message.get("tool_calls").and_then(|c| c.as_array())
                            {
                                for call in calls {
                                    if let Some(func) = call.get("function") {
                                        let name = func
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("unknown");
                                        let args = func
                                            .get("arguments")
                                            .cloned()
                                            .unwrap_or(serde_json::Value::Null);
                                        tool_calls.push(ToolCallLog {
                                            tool_name: name.to_string(),
                                            arguments: args,
                                            success: true,
                                            error: None,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                let entry = EvalLogEntry {
                    request_id: respond_id.clone(),
                    timestamp: crate::helpers::format_timestamp_rfc3339(),
                    role: "assistant".to_string(),
                    domain: "chat".to_string(),
                    source: "engine".to_string(),
                    input: log_input,
                    output,
                    reasoning: None,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    latency_ms: latency,
                    prompt_tokens: n_prompt_tokens,
                    completion_tokens: 0,
                    stop_reason: "stop".to_string(),
                    model: model_name.clone(),
                    is_stream: false,
                    retrieval_context: None,
                    expected_output: None,
                };
                let _ = get_eval_logger().log(&entry);
            }

            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}

fn js_engine_responses(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_RESPONSES_STREAM;

    let opts = cc.arg(0).as_object().ok_or_else(|| {
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
        .filter_map(crate::ai::responses::convert::input_item_to_chat_message)
        .collect();

    // Build prompt tokens
    let chat_tools: Option<Vec<async_openai::types::chat::ChatCompletionTools>> = tools
        .as_ref()
        .map(|t| crate::ai::responses::convert::responses_tools_to_chat_tools(t));

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

    let keep_tokens = {
        let engine = engine.lock().unwrap();
        let model = engine.model();
        compute_keep_tokens(model, &messages)
    };

    // Build task config
    let mut task_config = TaskConfig::new()
        .temperature(temperature.unwrap_or(0.75))
        .top_p(top_p.unwrap_or(1.0))
        .max_tokens(max_output_tokens.unwrap_or(engine.lock().unwrap().default_max_tokens()))
        .output_stream(is_stream)
        .keep_tokens(keep_tokens);

    if let Some(tools) = chat_tools.as_ref().filter(|t| !t.is_empty()) {
        task_config = task_config.grammar(GrammarConfig::new(
            engine.lock().unwrap().model(),
            tools,
            llama::ToolChoice::Auto,
        ));
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("res-{}", uuid::Uuid::new_v4().simple());
    let model_name = engine.lock().unwrap().model_name().to_string();
    let arch = engine.lock().unwrap().model().arch();

    // Push task to engine
    let rx = engine
        .lock()
        .unwrap()
        .push_task(prompt_tokens, task_config, output_prepend)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    if is_stream {
        // Streaming: AsyncIterableIterator
        let (item_tx, item_rx) = ctx.make_channel::<ChunkSSE>(8);
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
                arch,
            );
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}

fn js_engine_parseReasoning(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    Ok(JSValue::from(engine.lock().unwrap().parse_reasoning()))
}

fn js_engine_setParseReasoning(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;
    let v = cc.arg(0).to_bool()?;
    engine.lock().unwrap().set_parse_reasoning(v);
    Ok(JSValue::UNDEFINED)
}

fn js_engine_dispose(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let _ = JSEngine::take_payload_of(&mut this);

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
    FieldDef::GETSET(
        "parseReasoning",
        Some(js_engine_parseReasoning),
        Some(js_engine_setParseReasoning),
        0,
    ),
    FieldDef::FUNC("dispose", 0, js_engine_dispose, 0),
];

def_native_object!(
    "Engine",
    JSEngine,
    Arc<Mutex<Engine>>,
    CLASS_ENGINE_PROTO,
    None
);
