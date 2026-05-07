// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, sync::Arc, time::Duration};

use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
        ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionStreamOptions, ChatCompletionTool,
        ChatCompletionToolChoiceOption, ChatCompletionTools, CreateChatCompletionRequestArgs,
        CreateChatCompletionResponse, CreateChatCompletionStreamResponse, ReasoningEffort,
        ResponseFormat, ToolChoiceOptions,
    },
};
use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassCtorDef, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
};

use crate::{
    USER_CLASS_DEFS,
    class_llama_engine::{MAGIC_CHAT_ONCE, MAGIC_CHAT_STREAM},
};

/// Constructor: new OpenAI({apiKey, baseUrl, model?, timeout?})
fn js_client_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

    let mut config = OpenAIConfig::new().with_api_key(&api_key);

    let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
    if let Some(s) = base_url_val.as_string() {
        let url = s.content().encode_utf8(false);
        config = config.with_api_base(url.as_ref());
    }

    let timeout_val = opts_obj.get_property_str(ctx, "timeout")?;
    let timeout = if !timeout_val.is_undefined() {
        Some(Duration::from_secs(
            timeout_val.to_number(ctx)?.to_f64() as u64
        ))
    } else {
        None
    };

    let default_model: Option<String> = opts_obj
        .get_property_str(ctx, "model")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string());

    let client = Client::with_config(config);

    let opaque = Arc::into_raw(Arc::new((client, default_model))).cast::<c_void>();
    ctx.create_class_instance_with_opaque(JSLlamaClient::class_id(), opaque as _)
        .map_err(|e| {
            let p = e.other().unwrap();
            let _ = unsafe {
                Arc::from_raw(p.opaque().cast::<(Client<OpenAIConfig>, Option<String>)>())
            };
            -1
        })
        .map(|obj| obj.into())
}

type ClientState = (Client<OpenAIConfig>, Option<String>);

/// Get Arc<ClientState> from JS value (bumps strong count)
fn get_client_arc(ctx: &mut ScopedContext, val: &JSValue) -> Result<Arc<ClientState>, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSLlamaClient::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("OpenAI client was disposed");
        Err(-1)
    } else {
        let ptr = opaque.cast::<ClientState>();
        // SAFETY: opaque came from Arc::into_raw, bump strong count to create a new Arc
        unsafe { Arc::increment_strong_count(ptr) };
        Ok(unsafe { Arc::from_raw(ptr) })
    }
}

/// Parse JS messages array → Vec<ChatCompletionRequestMessage>
pub(crate) fn parse_messages(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Vec<ChatCompletionRequestMessage>, i32> {
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
            "system" => ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(content),
                ..Default::default()
            }),
            "user" => ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(content),
                ..Default::default()
            }),
            "assistant" => {
                ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                    content: if content.is_empty() {
                        None
                    } else {
                        Some(ChatCompletionRequestAssistantMessageContent::Text(content))
                    },
                    ..Default::default()
                })
            }
            "tool" => {
                let tc_id = obj
                    .get_property_str(ctx, "tool_call_id")?
                    .as_string()
                    .map(|s| s.content().encode_utf8(false).to_string())
                    .unwrap_or_default();
                ChatCompletionRequestMessage::Tool(ChatCompletionRequestToolMessage {
                    content: ChatCompletionRequestToolMessageContent::Text(content),
                    tool_call_id: tc_id,
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

/// Parse JS tools array → Vec<ChatCompletionTools>
pub(crate) fn parse_tools(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Vec<ChatCompletionTools>, i32> {
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

    serde_json::from_value::<Vec<ChatCompletionTools>>(serde_json::Value::Array(tools_json))
        .map_err(|e| {
            ctx.throw_type_error(&format!("invalid tool definition: {e}"));
            -1
        })
}

/// Parse JS tool_choice → ChatCompletionToolChoiceOption
pub(crate) fn parse_tool_choice(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<ChatCompletionToolChoiceOption, i32> {
    if let Some(s) = val.as_string() {
        let mode = s.content().encode_utf8(false);
        match mode.as_ref() {
            "auto" => Ok(ChatCompletionToolChoiceOption::Mode(
                ToolChoiceOptions::Auto,
            )),
            "required" => Ok(ChatCompletionToolChoiceOption::Mode(
                ToolChoiceOptions::Required,
            )),
            "none" => Ok(ChatCompletionToolChoiceOption::Mode(
                ToolChoiceOptions::None,
            )),
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
        serde_json::from_value::<ChatCompletionToolChoiceOption>(json_val).map_err(|e| {
            ctx.throw_type_error(&format!("invalid tool_choice: {e}"));
            -1
        })
    }
}

/// Parse JS response_format → ResponseFormat
pub(crate) fn parse_response_format(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<ResponseFormat, i32> {
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
pub(crate) fn parse_reasoning_effort(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<ReasoningEffort, i32> {
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

/// All parsed fields are set on the returned request; caller fills in messages and any engine-specific fields (idSlot, cachePrompt, includeUsage) separately.
pub(crate) struct ParsedChatOptions {
    pub model: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f32>,
    pub n: Option<u32>,
    pub stop: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub logit_bias: Option<std::collections::HashMap<String, i8>>,
    pub user: Option<String>,
    pub tools: Option<Vec<ChatCompletionTools>>,
    pub tool_choice: Option<ChatCompletionToolChoiceOption>,
    pub parallel_tool_calls: Option<bool>,
    pub response_format: Option<ResponseFormat>,
    pub stream_options: Option<ChatCompletionStreamOptions>,
    pub seed: Option<i64>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

pub(crate) fn parse_chat_options(
    ctx: &mut ScopedContext,
    opts: &Object,
) -> Result<ParsedChatOptions, i32> {
    let mut model = String::new();
    let mut temperature: Option<f32> = None;
    let mut max_tokens: Option<u32> = None;
    let mut top_p: Option<f32> = None;
    let mut n: Option<u32> = None;
    let mut stop: Option<Vec<String>> = None;
    let mut presence_penalty: Option<f32> = None;
    let mut frequency_penalty: Option<f32> = None;
    let mut logit_bias: Option<std::collections::HashMap<String, i8>> = None;
    let mut user: Option<String> = None;
    let mut tools: Option<Vec<ChatCompletionTools>> = None;
    let mut tool_choice: Option<ChatCompletionToolChoiceOption> = None;
    let mut parallel_tool_calls: Option<bool> = None;
    let mut response_format: Option<ResponseFormat> = None;
    let mut stream_options: Option<ChatCompletionStreamOptions> = None;
    let mut seed: Option<i64> = None;
    let mut reasoning_effort: Option<ReasoningEffort> = None;

    // model
    let v = opts.get_property_str(ctx, "model")?;
    if let Some(s) = v.as_string() {
        model = s.content().encode_utf8(false).to_string();
    }

    // temperature
    let v = opts.get_property_str(ctx, "temperature")?;
    if !v.is_undefined() {
        temperature = Some(v.to_number(ctx)?.to_f64() as f32);
    }

    // maxTokens
    let v = opts.get_property_str(ctx, "maxTokens")?;
    if !v.is_undefined() {
        max_tokens = Some(v.to_number(ctx)?.to_f64() as u32);
    }

    // topP
    let v = opts.get_property_str(ctx, "topP")?;
    if !v.is_undefined() {
        top_p = Some(v.to_number(ctx)?.to_f64() as f32);
    }

    // n
    let v = opts.get_property_str(ctx, "n")?;
    if !v.is_undefined() {
        n = Some(v.to_number(ctx)?.to_f64() as u32);
    }

    // stop
    let v = opts.get_property_str(ctx, "stop")?;
    if !v.is_undefined() {
        if let Some(s) = v.as_string() {
            stop = Some(vec![s.content().encode_utf8(false).to_string()]);
        } else if let Some(arr) = v.as_object_Array() {
            let mut words = Vec::with_capacity(arr.payload().len());
            for item in arr.payload().values() {
                if let Some(s) = item.as_string() {
                    words.push(s.content().encode_utf8(false).to_string());
                }
            }
            stop = Some(words);
        }
    }

    // presencePenalty
    let v = opts.get_property_str(ctx, "presencePenalty")?;
    if !v.is_undefined() {
        presence_penalty = Some(v.to_number(ctx)?.to_f64() as f32);
    }

    // frequencyPenalty
    let v = opts.get_property_str(ctx, "frequencyPenalty")?;
    if !v.is_undefined() {
        frequency_penalty = Some(v.to_number(ctx)?.to_f64() as f32);
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

    Ok(ParsedChatOptions {
        model,
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
    })
}

enum ChatReply {
    Chunk(CreateChatCompletionStreamResponse),
    Done,
    Error(String),
}

fn js_client_chat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client_arc = get_client_arc(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;

    // Single object argument
    let opts = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("chat() requires an options object");
        -1
    })?;

    // Parse messages (required)
    let messages_val = opts.get_property_str(ctx, "messages")?;
    let messages = parse_messages(ctx, &messages_val)?;

    // Parse all chat options into a ChatCompletionRequest
    let parsed = parse_chat_options(ctx, &opts)?;

    let model = if parsed.model.is_empty() {
        client_arc
            .1
            .clone()
            .unwrap_or_else(|| "default".to_string())
    } else {
        parsed.model
    };

    let mut req_builder = CreateChatCompletionRequestArgs::default();
    req_builder.model(&model).messages(messages);

    if let Some(v) = parsed.temperature {
        req_builder.temperature(v);
    }
    if let Some(v) = parsed.max_tokens {
        req_builder.max_tokens(v);
    }
    if let Some(v) = parsed.top_p {
        req_builder.top_p(v);
    }
    if let Some(v) = parsed.n {
        req_builder.n(v as u8);
    }
    if let Some(v) = parsed.stop {
        req_builder.stop(v);
    }
    if let Some(v) = parsed.presence_penalty {
        req_builder.presence_penalty(v);
    }
    if let Some(v) = parsed.frequency_penalty {
        req_builder.frequency_penalty(v);
    }
    if let Some(v) = parsed.logit_bias {
        req_builder.logit_bias(v);
    }
    if let Some(v) = parsed.user {
        req_builder.user(v);
    }
    if let Some(v) = parsed.tools {
        req_builder.tools(v);
    }
    if let Some(v) = parsed.tool_choice {
        req_builder.tool_choice(v);
    }
    if let Some(v) = parsed.parallel_tool_calls {
        req_builder.parallel_tool_calls(v);
    }
    if let Some(v) = parsed.response_format {
        req_builder.response_format(v);
    }
    if let Some(v) = parsed.stream_options {
        req_builder.stream_options(v);
    }
    if let Some(v) = parsed.seed {
        req_builder.seed(v);
    }
    if let Some(v) = parsed.reasoning_effort {
        req_builder.reasoning_effort(v);
    }

    let req = req_builder.build().map_err(|e| {
        ctx.throw_internal_error(&format!("failed to build request: {e}"));
        -1
    })?;

    let client = client_arc.0.clone();

    if is_stream {
        // Streaming: AsyncIterableIterator
        let result_slot = PromiseResultSlot::<ChatReply>::new();
        let slot = result_slot.clone();

        tokio::spawn(async move {
            async fn commit_slot(slot: &PromiseResultSlot<ChatReply>, mut msg: ChatReply) {
                loop {
                    match slot.commit(msg) {
                        Ok(()) => return,
                        Err(ret) => {
                            // slot is full, yield and do commit later
                            msg = ret;
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }

            match client.chat().create_stream(req).await {
                Ok(stream) => {
                    use futures_util::StreamExt;

                    let mut pin = std::pin::pin!(stream);
                    while let Some(result) = pin.next().await {
                        match result {
                            Ok(chunk) => {
                                commit_slot(&slot, ChatReply::Chunk(chunk)).await;
                            }
                            Err(e) => {
                                commit_slot(&slot, ChatReply::Error(e.to_string())).await;
                                break;
                            }
                        }
                    }

                    // stop iterate
                    commit_slot(&slot, ChatReply::Done).await;
                }
                Err(e) => {
                    commit_slot(&slot, ChatReply::Error(e.to_string())).await;
                }
            }
        });

        AsyncIterableIterator::create_with_result_slot(
            ctx,
            result_slot,
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
        let result_slot = PromiseResultSlot::<Result<CreateChatCompletionResponse, String>>::new();
        let promise =
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, result| -> JSResult {
                match result {
                    Ok(resp) => response_to_js(cx, &resp),
                    Err(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                }
            })?;

        tokio::spawn(async move {
            let result = client.chat().create(req).await.map_err(|e| e.to_string());
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}

/// dispose
fn js_client_dispose(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let payload = this
        .retrive_object_payload_mut(ctx, JSLlamaClient::class_id())?
        .other_mut()
        .unwrap();
    if !payload.opaque().is_null() {
        unsafe {
            let p = payload.replace_opaque(std::ptr::null_mut());
            drop(Arc::from_raw(p.cast::<ClientState>()));
        }
    }
    Ok(JSValue::UNDEFINED)
}

/// Convert CreateChatCompletionResponse to a JS object.
pub(crate) fn response_to_js(
    ctx: &mut ScopedContext,
    resp: &CreateChatCompletionResponse,
) -> JSResult {
    let val = serde_json::to_value(resp).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize response failed: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

/// Convert CreateChatCompletionStreamResponse to a JS object.
pub(crate) fn chunk_to_js(
    ctx: &mut ScopedContext,
    chunk: &CreateChatCompletionStreamResponse,
) -> JSResult {
    let val = serde_json::to_value(chunk).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize chunk failed: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

const CLASS_CLIENT_PROTO: &[FieldDef] = &[
    FieldDef::CALL("chat", 1, js_client_chat, MAGIC_CHAT_STREAM as i16),
    FieldDef::CALL("chatOnce", 1, js_client_chat, MAGIC_CHAT_ONCE as i16),
    FieldDef::CALL("dispose", 0, js_client_dispose, 0),
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
                CLASS_CLIENT_PROTO,
                Some(ClassCtorDef::new(js_client_constructor, 1, false)),
            )
            .on_dispose(|_, opaque: *mut c_void, _| {
                if !opaque.is_null() {
                    drop(unsafe { Arc::from_raw(opaque.cast::<ClientState>()) });
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
