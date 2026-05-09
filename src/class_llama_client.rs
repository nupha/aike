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
        ChatCompletionRequestUserMessageContent, CreateChatCompletionRequest,
        CreateChatCompletionResponse, CreateChatCompletionStreamResponse,
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
    class_llama_client_responses::{
        MAGIC_RESPONSES_ONCE, MAGIC_RESPONSES_STREAM, js_client_responses,
    },
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

    JSLlamaClient::new_instance(ctx, client, default_model).map(|obj| obj.into())
}

pub(crate) struct ClientState {
    pub client: Client<OpenAIConfig>,
    pub model: Option<String>,
}

/// Get Arc<ClientState> from JS value (bumps strong count)
pub(crate) fn get_client_arc(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Arc<ClientState>, i32> {
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

pub(crate) fn parse_chat_completion_options(
    ctx: &mut ScopedContext,
    opts: Object,
) -> Result<CreateChatCompletionRequest, i32> {
    let mut json_val = opts.to_serde_json(ctx).map_err(|_| {
        ctx.throw_type_error("failed to serialize options");
        -1
    })?;

    // Note:
    // 1) messages is parsed separately, inject empty array to satisfy deserialization
    // 2) model is required, set default value if missing
    if let Some(obj) = json_val.as_object_mut() {
        obj.insert("messages".to_string(), serde_json::Value::Array(Vec::new()));
        if !obj.contains_key("model") {
            obj.insert(
                "model".to_string(),
                serde_json::Value::String("default".to_string()),
            );
        }
    }

    let mut req: CreateChatCompletionRequest = serde_json::from_value(json_val).map_err(|e| {
        ctx.throw_type_error(&format!("invalid options: {e}"));
        -1
    })?;

    // // Apply defaults for fields where None means "use default"
    // req.reasoning_effort.get_or_insert_with(Default::default);

    // messages is parsed separately, clear it here
    req.messages = Vec::new();

    Ok(req)
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
    let opts_obj = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("chat() requires an options object");
        -1
    })?;

    // Parse messages (required)
    let messages_val = opts_obj.get_property_str(ctx, "messages")?;
    let messages = parse_messages(ctx, &messages_val)?;

    // Parse all chat options into a ChatCompletionRequest
    let mut req = parse_chat_completion_options(ctx, opts_obj)?;

    if req.model.is_empty() {
        req.model = client_arc
            .model
            .clone()
            .unwrap_or_else(|| "default".to_string());
    }
    req.messages = messages;

    let client = client_arc.client.clone();

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
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, data| -> JSResult {
                match data {
                    Ok(resp) => {
                        // convert Chat completion response to JS object
                        let val = serde_json::to_value(resp).map_err(|e| {
                            cx.throw_internal_error(&format!("serialize response failed: {e}"));
                            -1
                        })?;
                        JSValue::from_serde_json(cx, &val)
                    }
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
    FieldDef::CALL(
        "responses",
        1,
        js_client_responses,
        MAGIC_RESPONSES_STREAM as i16,
    ),
    FieldDef::CALL(
        "responsesOnce",
        1,
        js_client_responses,
        MAGIC_RESPONSES_ONCE as i16,
    ),
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

    pub fn new_instance(
        ctx: &mut ScopedContext,
        client: Client<OpenAIConfig>,
        model: Option<String>,
    ) -> Result<Object, i32> {
        let state = Arc::new(ClientState { client, model });
        let opaque = Arc::into_raw(state).cast::<c_void>() as *mut c_void;
        ctx.create_class_instance_with_opaque(Self::class_id(), opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Arc::from_raw(p.opaque().cast::<ClientState>()) };
                -1
            })
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_openai(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlamaClient::register(rt)
}
