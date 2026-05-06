use std::{
    cell::Cell,
    ffi::c_void,
    sync::{Arc, Mutex},
};

use linkme::distributed_slice;
use liter_llm::{
    ChatCompletionChunk, ChatCompletionResponse, Choice, FinishReason, FunctionCall,
    ReasoningEffort, StreamChoice, StreamDelta, ToolCall, ToolType,
    types::common::{AssistantMessage, ChatCompletionTool, ResponseFormat, ToolChoice, Usage},
};
use llama_cpp_ffi::ToolChoice as LarkToolChoice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_client::{
        chunk_to_js, parse_messages, parse_reasoning_effort, parse_response_format,
        parse_tool_choice, parse_tools,
    },
    class_llama_context::parse_tokens,
    engine::{Engine, EngineReply, GrammarConfig, OutputMode, StopReason, TaskConfig},
    helpers::{build_chat_prompt_tokens, compile_tools_grammar, now_unix_s, to_lark_tool_choice},
};

pub(crate) const ERR_CANCELLED: &str = "cancelled";
pub(crate) const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";

pub(super) struct EngineParams {
    pub idle_sleep_ms: u64,
    pub slot_match_threshold: f32,
    /// Default max tokens for generation; 0 means unlimited.
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

        let v = opts.get_property_str(ctx, "maxTokens")?;
        if !v.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n < 0 {
                ctx.throw_type_error("maxTokens must be >= 0");
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

fn get_engine<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<&'a Mutex<Engine>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaEngine::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("Engine was disposed");
        Err(-1)
    } else {
        let k = unsafe { &*opaque.cast::<Mutex<Engine>>() };
        Ok(k)
    }
}

pub(super) fn get_engine_arc(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Arc<Mutex<Engine>>, i32> {
    // opaque was created from Arc::into_raw(), so it points to the Engine
    // inside the Arc allocation. Increment the strong count so the returned
    // Arc can outlive the JS object.
    let engine_ref = get_engine(ctx, val)?;
    let ptr = engine_ref as *const Mutex<Engine>;
    unsafe { Arc::increment_strong_count(ptr) };
    Ok(unsafe { Arc::from_raw(ptr) })
}

const MAGIC_GENERATE: i32 = 0;
const MAGIC_GENERATE_STREAM: i32 = 1;
const MAGIC_CHAT: i32 = 2;
const MAGIC_CHAT_STREAM: i32 = 3;

fn js_engine_generate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    let arg0 = cc.arg(0);
    let tokens = parse_tokens(ctx, &arg0)?.into_owned();
    if tokens.is_empty() {
        ctx.throw_type_error("tokens cannot be empty");
        return Err(-1);
    }

    let mode = if cc.magic() == MAGIC_GENERATE {
        OutputMode::OneShot
    } else {
        OutputMode::Continuous
    };

    let rx = engine
        .lock()
        .unwrap()
        .push_task(tokens, TaskConfig::new().output_mode(mode))
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    if cc.magic() == MAGIC_GENERATE {
        // generate()
        let promise =
            Promise::create_with_receiver::<EngineReply>(ctx, rx, |cx, msg| -> JSResult {
                match msg {
                    EngineReply::Text(text) => cx
                        .create_string_from_utf8(text.into_bytes())
                        .map(JSValue::from),
                    EngineReply::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                    EngineReply::Cancelled => {
                        cx.throw_internal_error("task cancelled");
                        Err(-1)
                    }
                    EngineReply::Shutdown => {
                        cx.throw_internal_error(ERR_ENGINE_SHUTDOWN);
                        Err(-1)
                    }
                    EngineReply::Stop(_, _) => {
                        cx.throw_internal_error("unexpected message in oneshot mode");
                        Err(-1)
                    }
                }
            })?;
        Ok(promise.into())
    } else {
        // generateStream()
        AsyncIterableIterator::create_with_receiver(
            ctx,
            rx,
            |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    EngineReply::Text(text) => cx
                        .create_string_from_utf8(text.into_bytes())
                        .map(JSValue::from)
                        .map(Some),
                    EngineReply::Stop(_, _) => Ok(None),
                    EngineReply::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                    EngineReply::Cancelled => {
                        cx.throw_internal_error("task cancelled");
                        Err(-1)
                    }
                    EngineReply::Shutdown => {
                        cx.throw_internal_error(ERR_ENGINE_SHUTDOWN);
                        Err(-1)
                    }
                }
            },
        )
        .map(JSValue::from)
    }
}

/// Collect all EngineReply messages for non-streaming chat, building a ChatCompletionResponse json.
/// If `timeout` elapses without a message, returns a timeout error.
pub(super) fn collect_chat_response(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    include_usage: bool,
    n_prompt_tokens: usize,
) -> Result<serde_json::Value, String> {
    let mut full_text = String::new();

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Text(t)) => full_text.push_str(&t),
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                let finish_reason = match reason {
                    StopReason::MaxTokens => FinishReason::Length,
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                        FinishReason::Stop
                    }
                };

                // Detect tool calls in the generated text (e.g. <tool_call>...</tool_call>)
                let tool_calls = parse_tool_calls_from_text(&full_text);

                let resp = ChatCompletionResponse {
                    id: respond_id.to_string(),
                    object: "chat.completion".to_string(),
                    created: now_unix_s() as u64,
                    model: model_name.to_string(),
                    choices: vec![Choice {
                        index: 0,
                        message: AssistantMessage {
                            content: Some(full_text),
                            tool_calls,
                            ..Default::default()
                        },
                        finish_reason: Some(finish_reason),
                    }],
                    usage: if include_usage {
                        Some(Usage {
                            prompt_tokens: n_prompt_tokens as u64,
                            completion_tokens: n_completion_tokens as u64,
                            total_tokens: (n_prompt_tokens + n_completion_tokens) as u64,
                        })
                    } else {
                        None
                    },
                    ..Default::default()
                };

                return serde_json::to_value(resp).map_err(|e| e.to_string());
            }
            Ok(EngineReply::Cancelled) => return Err(ERR_CANCELLED.to_string()),
            Ok(EngineReply::Error(e)) => return Err(e),
            Ok(EngineReply::Shutdown) => return Err(ERR_ENGINE_SHUTDOWN.to_string()),
            Err(_) => return Err("stream timed out".to_string()),
        }
    }
}

/// Parse tool calls from generated text that contains `<tool_call>...</tool_call>` tags.
/// Returns `None` if no tool calls are found.
///
/// The model outputs `arguments` as a JSON object (Qwen/Hermes format):
///   {"name": "get_weather", "arguments": {"city": "Shanghai"}}
/// We serialize the `arguments` object to a JSON string to match the OpenAI
/// `FunctionCall` format where `arguments` is a string.
fn parse_tool_calls_from_text(text: &str) -> Option<Vec<ToolCall>> {
    let mut calls = Vec::<ToolCall>::new();
    let mut remaining = text;

    while let Some(start) = remaining.find("<tool_call>") {
        let content_start = start + "<tool_call>".len();
        let end = remaining[content_start..].find("</tool_call>")?;
        let json_str = &remaining[content_start..content_start + end];

        // First parse as generic Value to handle arguments as JSON object
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
            let name = value["name"].as_str().map(|s| s.to_string());
            let args_value = value["arguments"].clone();

            if let Some(name) = name {
                // Stringify the arguments object to a JSON string
                let Ok(arguments) = serde_json::to_string(&args_value) else {
                    eprint!("[warn] bad arguments in tool_call: {}", args_value);
                    return None;
                };

                calls.push(ToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                    call_type: ToolType::Function,
                    function: FunctionCall { name, arguments },
                });
            }
        }

        remaining = &remaining[content_start + end + "</tool_call>".len()..];
    }

    if !calls.is_empty() { Some(calls) } else { None }
}

pub(crate) enum ChatStreamItem {
    Chunk(ChatCompletionChunk),
    Done,
    Error(String),
}

/// Bridge crossbeam Receiver<EngineReply> for streaming.
/// Emits role delta as first chunk, content deltas for Text, and finish chunk for Stop.
/// Calls `on_item` for each ChatStreamItem produced.
/// If `timeout` elapses without a message, emits an error and stops.
pub(crate) fn bridge_chat_stream<F>(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    include_usage: bool,
    n_prompt_tokens: usize,
    mut on_item: F,
) where
    F: FnMut(ChatStreamItem) -> bool,
{
    let respond_id = respond_id.to_string();
    let model_name = model_name.to_string();
    let mut sent_started = false;
    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Text(t)) => {
                let created = now_unix_s() as u64;
                if !sent_started {
                    // First chunk: emit role delta
                    sent_started = true;
                    let first = ChatCompletionChunk {
                        id: respond_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model_name.clone(),
                        choices: vec![StreamChoice {
                            index: 0,
                            delta: StreamDelta {
                                role: Some("assistant".to_string()),
                                content: None,
                                ..Default::default()
                            },
                            finish_reason: None,
                        }],
                        ..Default::default()
                    };
                    if !on_item(ChatStreamItem::Chunk(first)) {
                        break;
                    }
                }
                let chunk = ChatCompletionChunk {
                    id: respond_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model_name.clone(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: StreamDelta {
                            role: None,
                            content: Some(t),
                            ..Default::default()
                        },
                        finish_reason: None,
                    }],
                    ..Default::default()
                };
                if !on_item(ChatStreamItem::Chunk(chunk)) {
                    break;
                }
            }
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                let last = ChatCompletionChunk {
                    id: respond_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created: now_unix_s() as u64,
                    model: model_name.clone(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: StreamDelta::default(),
                        finish_reason: Some(match reason {
                            StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                                FinishReason::Stop
                            }
                            StopReason::MaxTokens => FinishReason::Length,
                        }),
                    }],
                    usage: if include_usage {
                        Some(Usage {
                            prompt_tokens: n_prompt_tokens as u64,
                            completion_tokens: n_completion_tokens as u64,
                            total_tokens: (n_prompt_tokens + n_completion_tokens) as u64,
                        })
                    } else {
                        None
                    },
                    ..Default::default()
                };
                on_item(ChatStreamItem::Chunk(last));
                on_item(ChatStreamItem::Done);
                break;
            }
            Ok(EngineReply::Cancelled) => {
                on_item(ChatStreamItem::Error(ERR_CANCELLED.to_string()));
                break;
            }
            Ok(EngineReply::Error(e)) => {
                on_item(ChatStreamItem::Error(e));
                break;
            }
            Ok(EngineReply::Shutdown) => {
                on_item(ChatStreamItem::Error(ERR_ENGINE_SHUTDOWN.to_string()));
                break;
            }
            Err(_) => {
                on_item(ChatStreamItem::Error("stream timed out".to_string()));
                break;
            }
        }
    }
}

fn js_engine_chat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine(ctx, &this)?;

    // Parse messages (required, arg 0)
    let messages_val = cc.arg(0);
    let messages = parse_messages(ctx, &messages_val)?;

    // Parse options (optional, arg 1)
    let mut stream = false;
    let mut temperature = 0.75_f32;
    let mut top_p = 1.0_f32;
    let mut stop: Vec<String> = Vec::new();
    let mut tools: Option<Vec<ChatCompletionTool>> = None;
    let mut tool_choice: Option<ToolChoice> = None;
    let mut parallel_tool_calls: Option<bool> = None;
    let mut response_format: Option<ResponseFormat> = None;
    let mut reasoning_effort: Option<ReasoningEffort> = None;
    let mut id_slot: Option<usize> = None;
    let mut cache_prompt = false;
    let mut include_usage = false;
    let mut max_tokens = engine.lock().unwrap().default_max_tokens();

    if cc.length() > 1 {
        let opts_val = cc.arg(1);
        if !opts_val.is_undefined() && !opts_val.is_null() {
            let opts = opts_val.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("options must be object");
                -1
            })?;

            // stream
            let v = opts.get_property_str(ctx, "stream")?;
            if !v.is_undefined() {
                stream = v.to_bool()?;
            }

            // temperature
            let v = opts.get_property_str(ctx, "temperature")?;
            if !v.is_undefined() {
                temperature = v.to_number(ctx)?.to_f64() as f32;
            }

            // maxTokens
            let v = opts.get_property_str(ctx, "maxTokens")?;
            if !v.is_undefined() {
                let n = v.to_number(ctx)?.to_i32();
                if n < 0 {
                    ctx.throw_type_error("maxTokens must be >= 0");
                    return Err(-1);
                }
                max_tokens = n as usize;
            }

            // topP
            let v = opts.get_property_str(ctx, "topP")?;
            if !v.is_undefined() {
                top_p = v.to_number(ctx)?.to_f64() as f32;
            }

            // stop
            let v = opts.get_property_str(ctx, "stop")?;
            if !v.is_undefined() {
                if v.is_string() {
                    let s = v.to_string(ctx)?;
                    stop.push(s.content().encode_utf8(false).to_string());
                } else if let Some(arr) = v.as_object_Array() {
                    for item in arr.payload().values() {
                        if let Some(s) = item.as_string() {
                            stop.push(s.content().encode_utf8(false).to_string());
                        }
                    }
                }
            }

            // tools
            let v = opts.get_property_str(ctx, "tools")?;
            if !v.is_undefined() {
                tools = Some(parse_tools(ctx, &v)?);
            }

            // toolChoice
            let v = opts.get_property_str(ctx, "toolChoice")?;
            if !v.is_undefined() {
                tool_choice = Some(parse_tool_choice(ctx, &v)?);
            }

            // parallelToolCalls
            let v = opts.get_property_str(ctx, "parallelToolCalls")?;
            if !v.is_undefined() {
                parallel_tool_calls = Some(v.to_bool()?);
            }

            // responseFormat
            let v = opts.get_property_str(ctx, "responseFormat")?;
            if !v.is_undefined() {
                response_format = Some(parse_response_format(ctx, &v)?);
            }

            // reasoningEffort
            let v = opts.get_property_str(ctx, "reasoningEffort")?;
            if !v.is_undefined() {
                reasoning_effort = Some(parse_reasoning_effort(ctx, &v)?);
            }

            // idSlot
            let v = opts.get_property_str(ctx, "idSlot")?;
            if !v.is_undefined() {
                let n = v.to_number(ctx)?.to_i32();
                if n >= 0 {
                    id_slot = Some(n as usize);
                }
            }

            // cachePrompt
            let v = opts.get_property_str(ctx, "cachePrompt")?;
            if !v.is_undefined() {
                cache_prompt = v.to_bool()?;
            }

            // includeUsage
            let v = opts.get_property_str(ctx, "includeUsage")?;
            if !v.is_undefined() {
                include_usage = v.to_bool()?;
            }
        }
    }

    // Build prompt tokens
    let prompt_tokens = build_chat_prompt_tokens(
        engine.lock().unwrap().model(),
        &messages,
        tools.as_deref(),
        tool_choice.as_ref(),
        parallel_tool_calls,
        reasoning_effort.as_ref(),
        response_format.as_ref(),
    )
    .map_err(|e| {
        ctx.throw_internal_error(&format!("build prompt tokens failed: {e}"));
        -1
    })?;

    // Build TaskConfig
    let mut task_config = TaskConfig::new()
        .stop_words(stop)
        .max_tokens(max_tokens)
        .temperature(temperature)
        .top_p(top_p)
        .output_mode(if stream {
            OutputMode::Continuous
        } else {
            OutputMode::OneShot
        });

    // tools → grammar
    if let Some(tools) = tools.as_ref().filter(|t| !t.is_empty()) {
        let tc = tool_choice
            .as_ref()
            .map(to_lark_tool_choice)
            .unwrap_or(LarkToolChoice::Auto);

        if tc != LarkToolChoice::None {
            task_config = task_config.grammar(GrammarConfig {
                lark_grammar: compile_tools_grammar(tools, tc),
                tool_choice: tc,
            });
        }
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let model_name = engine.lock().unwrap().model_name().to_string();
    let mode = cc.magic();

    let rx = engine
        .lock()
        .unwrap()
        .push_task_with_params(prompt_tokens, task_config, id_slot, cache_prompt)
        .map_err(|e| {
            ctx.throw_internal_error(&e.to_string());
            -1
        })?;

    if mode == MAGIC_CHAT {
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
            let result = collect_chat_response(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                include_usage,
                n_prompt_tokens,
            );
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    } else {
        // Streaming: AsyncIterableIterator
        let (bridge_tx, bridge_rx) = crossbeam::channel::bounded::<ChatStreamItem>(8);
        std::thread::spawn(move || {
            bridge_chat_stream(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                include_usage,
                n_prompt_tokens,
                |item| bridge_tx.send(item).is_ok(),
            );
        });

        AsyncIterableIterator::create_with_receiver(
            ctx,
            bridge_rx,
            |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    ChatStreamItem::Chunk(chunk) => chunk_to_js(cx, &chunk).map(Some),
                    ChatStreamItem::Done => Ok(None),
                    ChatStreamItem::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                }
            },
        )
        .map(JSValue::from)
    }
}

fn js_engine_httpServe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let engine = get_engine_arc(ctx, &this)?;

    // Parse options object
    let mut host = "0.0.0.0".to_string();
    let mut port: u16 = 8080;

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
        let res = Engine::http_serve(engine.clone(), &host, port, shutdown_rx).await;

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

fn js_engine_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaEngine::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Arc::from_raw(p.cast::<Engine>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

const CLASS_ENGINE_PROTO: &[FieldDef] = &[
    FieldDef::CALL("chat", 2, js_engine_chat, MAGIC_CHAT as i16),
    FieldDef::CALL("chatStream", 2, js_engine_chat, MAGIC_CHAT_STREAM as i16),
    FieldDef::CALL("generate", 1, js_engine_generate, MAGIC_GENERATE as i16),
    FieldDef::CALL(
        "generateStream",
        1,
        js_engine_generate,
        MAGIC_GENERATE_STREAM as i16,
    ),
    FieldDef::CALL("httpServe", 1, js_engine_httpServe, 0),
    FieldDef::CALL("stopServe", 0, js_engine_stopServe, 0),
    FieldDef::GETSET("isServing", Some(js_engine_isServing), None, 0),
    FieldDef::CALL("dispose", 0, js_engine_dispose, 0),
];

pub struct JSLlamaEngine;

impl JSLlamaEngine {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new("Engine", CLASS_ENGINE_PROTO, None).on_dispose(
                |_, opaque: *mut c_void, _| {
                    if !opaque.is_null() {
                        drop(unsafe { Arc::from_raw(opaque.cast::<Engine>()) });
                    }
                },
            ),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub(super) fn new_instance(
        ctx: &mut ScopedContext,
        engine: Arc<Mutex<Engine>>,
    ) -> Result<Object, i32> {
        let opaque = Arc::into_raw(engine) as *mut c_void;

        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Arc::from_raw(p.opaque().cast::<Mutex<Engine>>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_engine(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaEngine::register(rt)
}
