// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, sync::Arc, time::Duration};

use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{CreateChatCompletionResponse, CreateChatCompletionStreamResponse},
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
    helpers::parse_chat_completion_req,
};

/// Constructor: new OpenAI({apiKey, baseUrl, model?, timeout?})
fn js_client_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let opts = cc.arg(0);
    let opts_obj = opts.as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("OpenAI requires options object");
        -1
    })?;

    let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
    let base_url = if let Some(s) = base_url_val.as_string() {
        s.content().encode_utf8(false).to_string()
    } else {
        "https://api.openai.com/v1".to_string()
    };

    let api_key = opts_obj
        .get_property_str(ctx, "apiKey")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(|| "".to_string());

    let mut config = OpenAIConfig::new()
        .with_api_key(&api_key)
        .with_api_base(&base_url);

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

    JSLlamaClient::new_instance(ctx, client, default_model, base_url, api_key).map(|obj| obj.into())
}

pub(crate) struct ClientState {
    pub client: Client<OpenAIConfig>,
    pub model_name: Option<String>,
    pub base_url: String,
    pub api_key: String,
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

enum ChatReply {
    Chunk(serde_json::Value), // Use raw JSON to preserve extension fields
    Done,
    Error(String),
}

fn js_client_chatCompletions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client_arc = get_client_arc(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;

    // Single object argument
    let opts_obj = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("chat() requires an options object");
        -1
    })?;

    // Parse all chat options into a ChatCompletionRequest
    let mut req = opts_obj.to_serde_json(ctx).and_then(|v| {
        parse_chat_completion_req(v).map_err(|_| {
            ctx.throw_type_error("failed to serialize options");
            -1
        })
    })?;

    if req.model.is_empty() {
        req.model = client_arc
            .model_name
            .clone()
            .unwrap_or_else(|| "".to_string());
    }

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

            // Use reqwest to make HTTP request and parse SSE manually
            let base_url = client_arc.base_url.clone();
            let api_key = client_arc.api_key.clone();
            let url = format!("{}/chat/completions", base_url);

            let http_client = reqwest::Client::new();
            let request_body = serde_json::to_string(&req).unwrap_or_default();

            match http_client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key))
                .body(request_body)
                .send()
                .await
            {
                Ok(response) => {
                    use futures_util::StreamExt;
                    use tokio::io::AsyncBufReadExt;
                    use tokio_util::io::StreamReader;

                    // Convert response bytes to AsyncRead
                    let byte_stream = response
                        .bytes_stream()
                        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
                    let reader = StreamReader::new(byte_stream);
                    let mut lines = tokio::io::BufReader::new(reader).lines();

                    while let Ok(Some(line)) = lines.next_line().await {
                        let line = line.trim().to_string();
                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                break;
                            }

                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                // Check for error
                                if json.get("error").is_some() {
                                    let err_msg = json["error"]["message"]
                                        .as_str()
                                        .unwrap_or(data)
                                        .to_string();
                                    commit_slot(&slot, ChatReply::Error(err_msg)).await;
                                    break;
                                }

                                commit_slot(&slot, ChatReply::Chunk(json)).await;
                            }
                        }
                    }

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
        // Non-streaming: Promise - Use reqwest to preserve extension fields
        let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
        let promise =
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, data| -> JSResult {
                match data {
                    Ok(resp) => JSValue::from_serde_json(cx, &resp),
                    Err(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                }
            })?;

        let base_url = client_arc.base_url.clone();
        let api_key = client_arc.api_key.clone();

        tokio::spawn(async move {
            let url = format!("{}/chat/completions", base_url);
            let http_client = reqwest::Client::new();
            let request_body = serde_json::to_string(&req).unwrap_or_default();

            let result = match http_client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key))
                .body(request_body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if status.is_success() {
                        match serde_json::from_str::<serde_json::Value>(&body) {
                            Ok(json) => {
                                // Check for error in response
                                if json.get("error").is_some() {
                                    let err_msg = json["error"]["message"]
                                        .as_str()
                                        .unwrap_or(&body)
                                        .to_string();
                                    Err(err_msg)
                                } else {
                                    Ok(json)
                                }
                            }
                            Err(e) => Err(format!("Failed to parse JSON: {}", e)),
                        }
                    } else {
                        Err(format!("HTTP {} : {}", status, body))
                    }
                }
                Err(e) => Err(e.to_string()),
            };

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
pub(crate) fn chunk_to_js(ctx: &mut ScopedContext, chunk: &serde_json::Value) -> JSResult {
    // chunk is already a JSON Value, just convert it to JS
    JSValue::from_serde_json(ctx, chunk)
}

const CLASS_CLIENT_PROTO: &[FieldDef] = &[
    FieldDef::CALL(
        "chat",
        1,
        js_client_chatCompletions,
        MAGIC_CHAT_STREAM as i16,
    ),
    FieldDef::CALL(
        "chatOnce",
        1,
        js_client_chatCompletions,
        MAGIC_CHAT_ONCE as i16,
    ),
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
        base_url: String,
        api_key: String,
    ) -> Result<Object, i32> {
        let state = Arc::new(ClientState {
            client,
            model_name: model,
            base_url,
            api_key,
        });
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
