// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, sync::Arc, time::Duration};

use linkme::distributed_slice;
use liter_llm::types::common::{
    AssistantMessage, ChatCompletionTool, Message, ResponseFormat, StopSequence, SystemMessage,
    ToolChoice, ToolChoiceMode, ToolMessage, UserContent, UserMessage,
};
use liter_llm::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ClientConfigBuilder,
    DefaultClient, LlmClient, ReasoningEffort, StreamOptions,
};
use zuqe::{
    CallContext, ClassCtorDef, ClassDef, ClassID, FieldDef, JSResult, JSValue, Runtime,
    ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise},
};

use crate::USER_CLASS_DEFS;

/// Constructor: new OpenAI({apiKey, baseUrl, model?, timeout?})
fn js_openai_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let opts = cc.arg(0);
    let opts_obj = opts.as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("OpenAI requires options object");
        -1
    })?;

    let api_key = opts_obj
        .get_property_str(ctx, "apiKey")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|| "".to_string());

    let mut config_builder = ClientConfigBuilder::new(&api_key);

    let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
    if let Some(s) = base_url_val.as_string() {
        let url = s.content().encode_utf8(false);
        config_builder = config_builder.base_url(url.as_ref());
    }

    let timeout_val = opts_obj.get_property_str(ctx, "timeout")?;
    if !timeout_val.is_undefined() {
        let secs = timeout_val.to_number(ctx)?.to_f64() as u64;
        config_builder = config_builder.timeout(Duration::from_secs(secs));
    }

    // Optional default model
    let default_model: Option<String> = opts_obj
        .get_property_str(ctx, "model")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string());

    let client =
        DefaultClient::new(config_builder.build(), default_model.as_deref()).map_err(|e| {
            ctx.throw_internal_error(&format!("failed to create OpenAI client: {e}"));
            -1
        })?;

    let opaque = Arc::into_raw(Arc::new(client)).cast::<c_void>();
    ctx.create_class_instance_with_opaque(JSLlamaClient::class_id(), opaque as _)
        .map_err(|e| {
            let p = e.other().unwrap();
            let _ = unsafe { Arc::from_raw(p.opaque().cast::<DefaultClient>()) };
            -1
        })
        .map(|obj| obj.into())
}

/// Get Arc<DefaultClient> from JS value (bumps strong count)
fn get_client_arc(ctx: &mut ScopedContext, val: &JSValue) -> Result<Arc<DefaultClient>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaClient::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("OpenAI client was disposed");
        Err(-1)
    } else {
        let ptr = opaque.cast::<DefaultClient>();
        // SAFETY: opaque came from Arc::into_raw, bump strong count to create a new Arc
        unsafe { Arc::increment_strong_count(ptr) };
        Ok(unsafe { Arc::from_raw(ptr) })
    }
}

/// Parse JS messages array → Vec<Message>
fn parse_messages(ctx: &mut ScopedContext, val: &JSValue) -> Result<Vec<Message>, i32> {
    let arr = val.as_object_Array().copied().ok_or_else(|| {
        ctx.throw_type_error("messages must be an array");
        -1
    })?;

    if arr.payload().is_empty() {
        ctx.throw_type_error("messages must not be empty");
        return Err(-1);
    }

    let mut messages = Vec::with_capacity(arr.payload().len());
    for item in arr.payload().values() {
        let obj = item.as_object().copied().ok_or_else(|| {
            ctx.throw_type_error("messages[] item must be object");
            -1
        })?;

        let role_val = obj.get_property_str(ctx, "role")?;
        let role = role_val
            .as_string()
            .map(|s| s.content().encode_utf8(false).to_string())
            .ok_or_else(|| {
                ctx.throw_type_error("messages[].role must be string");
                -1
            })?;

        let content_val = obj.get_property_str(ctx, "content")?;
        let content = if content_val.is_undefined() || content_val.is_null() {
            String::new()
        } else if let Some(s) = content_val.as_string() {
            s.content().encode_utf8(false).to_string()
        } else {
            content_val
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string()
        };

        let msg = match role.as_str() {
            "system" => Message::System(SystemMessage {
                content: content.into(),
                name: None,
            }),
            "user" => Message::User(UserMessage {
                content: UserContent::Text(content.into()),
                name: None,
            }),
            "assistant" => Message::Assistant(AssistantMessage {
                content: if content.is_empty() {
                    None
                } else {
                    Some(content.into())
                },
                name: None,
                tool_calls: None,
                refusal: None,
                function_call: None,
            }),
            "tool" => {
                let tc_id = obj
                    .get_property_str(ctx, "tool_call_id")?
                    .as_string()
                    .map(|s| s.content().encode_utf8(false).to_string())
                    .unwrap_or_default();
                Message::Tool(ToolMessage {
                    content: content.into(),
                    tool_call_id: tc_id,
                    name: None,
                })
            }
            _ => {
                ctx.throw_type_error(&format!("unsupported role: {role}"));
                return Err(-1);
            }
        };
        messages.push(msg);
    }
    Ok(messages)
}

/// Parse JS tools array → Vec<ChatCompletionTool>
fn parse_tools(ctx: &mut ScopedContext, val: &JSValue) -> Result<Vec<ChatCompletionTool>, i32> {
    let arr = val.as_object_Array().copied().ok_or_else(|| {
        ctx.throw_type_error("tools must be an array");
        -1
    })?;

    // Convert JS array to serde_json::Value array, then deserialize
    let mut tools_json = Vec::with_capacity(arr.payload().len());
    for item in arr.payload().values() {
        let json_val = item.to_serde_json(ctx).map_err(|_| {
            ctx.throw_type_error("failed to serialize tool definition");
            -1
        })?;
        tools_json.push(json_val);
    }

    serde_json::from_value::<Vec<ChatCompletionTool>>(serde_json::Value::Array(tools_json)).map_err(
        |e| {
            ctx.throw_type_error(&format!("invalid tool definition: {e}"));
            -1
        },
    )
}

/// Parse JS tool_choice → ToolChoice
fn parse_tool_choice(ctx: &mut ScopedContext, val: &JSValue) -> Result<ToolChoice, i32> {
    if let Some(s) = val.as_string() {
        let mode = s.content().encode_utf8(false);
        match mode.as_ref() {
            "auto" => Ok(ToolChoice::Mode(ToolChoiceMode::Auto)),
            "required" => Ok(ToolChoice::Mode(ToolChoiceMode::Required)),
            "none" => Ok(ToolChoice::Mode(ToolChoiceMode::None)),
            _ => {
                ctx.throw_type_error(&format!("invalid tool_choice: {mode}"));
                Err(-1)
            }
        }
    } else {
        // Object form: {type: "function", function: {name: "xxx"}}
        let json_val = val.to_serde_json(ctx).map_err(|_| {
            ctx.throw_type_error("failed to serialize tool_choice");
            -1
        })?;
        serde_json::from_value::<ToolChoice>(json_val).map_err(|e| {
            ctx.throw_type_error(&format!("invalid tool_choice: {e}"));
            -1
        })
    }
}

/// Parse JS response_format → ResponseFormat
fn parse_response_format(ctx: &mut ScopedContext, val: &JSValue) -> Result<ResponseFormat, i32> {
    let json_val = val.to_serde_json(ctx).map_err(|_| {
        ctx.throw_type_error("failed to serialize response_format");
        -1
    })?;
    serde_json::from_value::<ResponseFormat>(json_val).map_err(|e| {
        ctx.throw_type_error(&format!("invalid response_format: {e}"));
        -1
    })
}

/// Parse JS reasoning_effort string → ReasoningEffort
fn parse_reasoning_effort(ctx: &mut ScopedContext, val: &JSValue) -> Result<ReasoningEffort, i32> {
    let s = val.as_string().ok_or_else(|| {
        ctx.throw_type_error("reasoningEffort must be a string");
        -1
    })?;
    let v = s.content().encode_utf8(false);
    match v.as_ref() {
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        _ => {
            ctx.throw_type_error(&format!("invalid reasoningEffort: {v}"));
            Err(-1)
        }
    }
}

enum ChatReply {
    Chunk(ChatCompletionChunk),
    Done,
    Error(String),
}

/// chat({messages, model?, stream?, temperature?, maxTokens?, topP?, stop?,
///       n?, presencePenalty?, frequencyPenalty?, logitBias?, user?,
///       tools?, toolChoice?, parallelToolCalls?, responseFormat?,
///       streamOptions?, seed?, reasoningEffort?})
fn js_openai_chat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client_arc(ctx, &this)?;

    // Single object argument
    let opts = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("chat() requires an options object");
        -1
    })?;

    // Parse messages (required)
    let messages_val = opts.get_property_str(ctx, "messages")?;
    let messages = parse_messages(ctx, &messages_val)?;

    // Optional params with camelCase JS naming
    let mut model = String::from("default");
    let mut stream = false;
    let mut temperature: Option<f64> = None;
    let mut max_tokens: Option<u64> = None;
    let mut top_p: Option<f64> = None;
    let mut stop: Option<StopSequence> = None;
    let mut n: Option<u32> = None;
    let mut presence_penalty: Option<f64> = None;
    let mut frequency_penalty: Option<f64> = None;
    let mut logit_bias: Option<std::collections::BTreeMap<String, f64>> = None;
    let mut user: Option<String> = None;
    let mut tools: Option<Vec<ChatCompletionTool>> = None;
    let mut tool_choice: Option<ToolChoice> = None;
    let mut parallel_tool_calls: Option<bool> = None;
    let mut response_format: Option<ResponseFormat> = None;
    let mut stream_options: Option<StreamOptions> = None;
    let mut seed: Option<i64> = None;
    let mut reasoning_effort: Option<ReasoningEffort> = None;

    // model
    let v = opts.get_property_str(ctx, "model")?;
    if let Some(s) = v.as_string() {
        model = s.content().encode_utf8(false).to_string();
    }

    // stream
    let v = opts.get_property_str(ctx, "stream")?;
    if !v.is_undefined() {
        stream = v.to_bool()?;
    }

    // temperature
    let v = opts.get_property_str(ctx, "temperature")?;
    if !v.is_undefined() {
        temperature = Some(v.to_number(ctx)?.to_f64());
    }

    // maxTokens
    let v = opts.get_property_str(ctx, "maxTokens")?;
    if !v.is_undefined() {
        max_tokens = Some(v.to_number(ctx)?.to_f64() as u64);
    }

    // topP
    let v = opts.get_property_str(ctx, "topP")?;
    if !v.is_undefined() {
        top_p = Some(v.to_number(ctx)?.to_f64());
    }

    // stop
    let v = opts.get_property_str(ctx, "stop")?;
    if !v.is_undefined() {
        if let Some(s) = v.as_string() {
            stop = Some(StopSequence::Single(
                s.content().encode_utf8(false).to_string(),
            ));
        } else if let Some(arr) = v.as_object_Array() {
            let mut words = Vec::with_capacity(arr.payload().len());
            for item in arr.payload().values() {
                if let Some(s) = item.as_string() {
                    words.push(s.content().encode_utf8(false).to_string());
                }
            }
            stop = Some(StopSequence::Multiple(words));
        }
    }

    // n
    let v = opts.get_property_str(ctx, "n")?;
    if !v.is_undefined() {
        n = Some(v.to_number(ctx)?.to_f64() as u32);
    }

    // presencePenalty
    let v = opts.get_property_str(ctx, "presencePenalty")?;
    if !v.is_undefined() {
        presence_penalty = Some(v.to_number(ctx)?.to_f64());
    }

    // frequencyPenalty
    let v = opts.get_property_str(ctx, "frequencyPenalty")?;
    if !v.is_undefined() {
        frequency_penalty = Some(v.to_number(ctx)?.to_f64());
    }

    // logitBias
    let v = opts.get_property_str(ctx, "logitBias")?;
    if !v.is_undefined() {
        let json_val = v.to_serde_json(ctx).map_err(|_| {
            ctx.throw_type_error("logitBias must be an object");
            -1
        })?;
        logit_bias = serde_json::from_value(json_val).map_err(|e| {
            ctx.throw_type_error(&format!("invalid logitBias: {e}"));
            -1
        })?;
    }

    // user
    let v = opts.get_property_str(ctx, "user")?;
    if let Some(s) = v.as_string() {
        user = Some(s.content().encode_utf8(false).to_string());
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

    // streamOptions
    let v = opts.get_property_str(ctx, "streamOptions")?;
    if !v.is_undefined() {
        let json_val = v.to_serde_json(ctx).map_err(|_| {
            ctx.throw_type_error("streamOptions must be an object");
            -1
        })?;
        stream_options = serde_json::from_value(json_val).map_err(|e| {
            ctx.throw_type_error(&format!("invalid streamOptions: {e}"));
            -1
        })?;
    }

    // seed
    let v = opts.get_property_str(ctx, "seed")?;
    if !v.is_undefined() {
        seed = Some(v.to_number(ctx)?.to_f64() as i64);
    }

    // reasoningEffort
    let v = opts.get_property_str(ctx, "reasoningEffort")?;
    if !v.is_undefined() {
        reasoning_effort = Some(parse_reasoning_effort(ctx, &v)?);
    }

    let req = ChatCompletionRequest {
        model,
        messages,
        temperature,
        max_tokens,
        top_p,
        n,
        stop,
        presence_penalty,
        frequency_penalty,
        logit_bias,
        user,
        tools,
        tool_choice,
        parallel_tool_calls,
        response_format,
        stream_options,
        seed,
        reasoning_effort,
        ..Default::default()
    };

    if stream {
        // Streaming: AsyncIterableIterator
        let (tx, rx) = crossbeam::channel::bounded::<ChatReply>(1);

        tokio::spawn(async move {
            match client.chat_stream(req).await {
                Ok(stream) => {
                    use futures_util::StreamExt;

                    let mut pin = std::pin::pin!(stream);
                    while let Some(result) = pin.next().await {
                        match result {
                            Ok(chunk) => {
                                if tx.send(ChatReply::Chunk(chunk)).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(ChatReply::Error(e.to_string()));
                                break;
                            }
                        }
                    }
                    let _ = tx.send(ChatReply::Done);
                }
                Err(e) => {
                    let _ = tx.send(ChatReply::Error(e.to_string()));
                }
            }
        });

        AsyncIterableIterator::create_with_receiver(
            ctx,
            rx,
            |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    ChatReply::Chunk(chunk) => chunk_to_js(cx, &chunk).map(Some),
                    ChatReply::Done => Ok(None),
                    ChatReply::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                }
            },
        )
        .map(JSValue::from)
    } else {
        // Non-streaming: Promise
        let (tx, rx) = crossbeam::channel::bounded::<Result<ChatCompletionResponse, String>>(1);

        tokio::spawn(async move {
            let result = client.chat(req).await.map_err(|e| e.to_string());
            let _ = tx.send(result);
        });

        Promise::create_with_receiver(ctx, rx, |cx, result| -> JSResult {
            match result {
                Ok(resp) => response_to_js(cx, &resp),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })
        .map(JSValue::from)
    }
}

/// dispose
fn js_openai_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaClient::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Arc::from_raw(p.cast::<DefaultClient>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

/// Convert ChatCompletionResponse to a JS object.
fn response_to_js(ctx: &mut ScopedContext, resp: &ChatCompletionResponse) -> JSResult {
    let val = serde_json::to_value(resp).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize response failed: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

/// Convert ChatCompletionChunk to a JS object.
fn chunk_to_js(ctx: &mut ScopedContext, chunk: &ChatCompletionChunk) -> JSResult {
    let val = serde_json::to_value(chunk).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize chunk failed: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

const CLASS_OPENAI_PROTO: &[FieldDef] = &[
    FieldDef::CALL("chat", 1, js_openai_chat, 0),
    FieldDef::CALL("dispose", 0, js_openai_dispose, 0),
];

pub struct JSLlamaClient;

impl JSLlamaClient {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new(
                "Client",
                CLASS_OPENAI_PROTO,
                Some(ClassCtorDef::new(js_openai_constructor, 1, false)),
            )
            .on_dispose(|_, opaque: *mut c_void, _| {
                if !opaque.is_null() {
                    drop(unsafe { Arc::from_raw(opaque.cast::<DefaultClient>()) });
                }
            }),
        )?;
        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_openai(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaClient::register(rt)
}
