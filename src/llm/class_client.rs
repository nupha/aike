// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, sync::Arc, time::Duration};

use futures_util::StreamExt;
use linkme::distributed_slice;
use reqwest::header::HeaderMap;
use tokio::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
    def_native_object,
};

use crate::{
    USER_CLASS_DEFS, get_llm_api_key, get_llm_base_url, get_llm_model,
    helpers::json_keys_camel_to_snake,
};

use super::{
    class_client_response::{MAGIC_RESPONSES_ONCE, MAGIC_RESPONSES_STREAM, js_client_responses},
    class_engine::{MAGIC_CHAT_ONCE, MAGIC_CHAT_STREAM},
};

#[derive(Debug)]
pub(crate) struct ClientPayload {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) org_id: String,
    pub(crate) project_id: String,
    pub(crate) custom_headers: HeaderMap,
    pub(crate) model_name: Option<String>,
    pub(crate) buffered_tool_call: bool,
    /// default extra_body field for openai request
    pub(crate) extra_body: serde_json::Value,
    /// Request timeout. `None` = no timeout (reqwest default, infinite wait).
    pub(crate) timeout: Option<Duration>,
}

pub(crate) fn get_client(ctx: &mut ScopedContext, val: JSValue) -> Result<Arc<ClientPayload>, i32> {
    let ob = JSLlmClient::try_from(val).unwrap();
    if let Some(c) = ob.payload() {
        Ok(c.clone())
    } else {
        ctx.throw_type_error("Client was disposed");
        Err(-1)
    }
}

/// Constructor: new Client({apiKey, baseUrl, model?, timeout?})
fn js_client_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    let opts = cc.arg(0);
    let opts_obj = opts.as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("requires options object");
        -1
    })?;

    let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
    let base_url = if let Some(s) = base_url_val.as_string() {
        s.content().encode_utf8(false).to_string()
    } else {
        get_llm_base_url()
    };

    let api_key = opts_obj
        .get_property_str(ctx, "apiKey")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_else(get_llm_api_key);

    let default_model: Option<String> = opts_obj
        .get_property_str(ctx, "model")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string());

    // extra_body
    let extra_body: serde_json::Value = match opts_obj.get_property_str(ctx, "extraBody")? {
        JSValue::NULL => serde_json::Value::Null,
        JSValue::UNDEFINED => {
            let s = std::env::var("AIKE_LLM_EXTRA_BODY").unwrap_or_default();
            if !s.is_empty() {
                if let Ok(o) = serde_json::from_str(&s) {
                    o
                } else {
                    eprintln!("[WARN] envvar AIKE_LLM_EXTRA_BODY is invalid JSON");
                    serde_json::Value::Null
                }
            } else {
                serde_json::Value::Null
            }
        }
        JSValue::Object(o) => o.to_serde_json(ctx)?,
        _ => {
            ctx.throw_type_error("extraBody must be one of undefined, null or object");
            return Err(-1);
        }
    };
    debug_assert!(extra_body.is_null() || extra_body.is_object());

    let buffered_tool_call = true;
    // let buffered_tool_call = opts_obj
    //     .get_property_str(ctx, "bufferedToolCall")?
    //     .as_bool()
    //     .unwrap_or(false);

    let timeout_ms = opts_obj.get_property_str(ctx, "timeout")?;
    let timeout = if !timeout_ms.is_undefined() {
        Some(
            timeout_ms
                .to_number(ctx)
                .map(|n| Duration::from_millis(n.to_u32() as u64))?,
        )
    } else {
        None
    };

    JSLlmClient::new_instance(
        ctx,
        new_target,
        Arc::new(ClientPayload {
            model_name: default_model,
            base_url,
            api_key,
            org_id: String::new(),
            project_id: String::new(),
            custom_headers: HeaderMap::new(),
            buffered_tool_call,
            extra_body,
            timeout,
        }),
    )
    .map(|obj| obj.into())
}

/// Accumulates tool_call deltas until complete, then emits a single chunk.
struct ToolCallBuffer {
    idx: usize,
    id: Option<String>,
    r#type: Option<String>,
    name: Option<String>,
    arguments: String,
    // Original chunk metadata preserved for completeness
    chunk_id: Option<String>,
    chunk_object: Option<String>,
    chunk_created: Option<i64>,
    chunk_model: Option<String>,
}

impl ToolCallBuffer {
    fn new(idx: usize) -> Self {
        Self {
            idx,
            id: None,
            r#type: None,
            name: None,
            arguments: String::new(),
            chunk_id: None,
            chunk_object: None,
            chunk_created: None,
            chunk_model: None,
        }
    }

    fn apply_delta(&mut self, tc: &serde_json::Value, original: &serde_json::Value) {
        if let Some(id) = tc["id"].as_str() {
            self.id = Some(id.to_owned());
        }
        if let Some(t) = tc["type"].as_str() {
            self.r#type = Some(t.to_owned());
        }
        if let Some(name) = tc["function"]["name"].as_str() {
            self.name = Some(name.to_owned());
        }
        if let Some(args) = tc["function"]["arguments"].as_str() {
            self.arguments.push_str(args);
        }
        // Capture chunk-level metadata from the first chunk that triggers tool_call accumulation
        if self.chunk_id.is_none() {
            self.chunk_id = original["id"].as_str().map(String::from);
            self.chunk_object = original["object"].as_str().map(String::from);
            self.chunk_created = original["created"].as_i64();
            self.chunk_model = original["model"].as_str().map(String::from);
        }
    }

    fn into_chunk(self) -> serde_json::Value {
        serde_json::json!({
            "id": self.chunk_id,
            "object": self.chunk_object,
            "created": self.chunk_created,
            "model": self.chunk_model,
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": self.idx,
                        "id": self.id,
                        "type": self.r#type,
                        "function": {
                            "name": self.name,
                            "arguments": self.arguments,
                        }
                    }]
                },
                "finish_reason": null
            }]
        })
    }
}

enum ChatReply {
    Chunk(serde_json::Value), // Use raw JSON to preserve extension fields
    Done,
    Error(String),
}

fn js_client_chatCompletions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, this)?;
    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;

    // Single object argument
    let opts_obj = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("chat() requires an options object");
        -1
    })?;

    // Convert JS options to raw JSON, preserving all fields (including extension
    // fields like reasoning_content) that async-openai typed structs would drop.
    // Client is a remote API proxy — it should forward everything, not filter.
    let mut req_json = opts_obj.to_serde_json(ctx)?;

    // camelCase → snake_case for OpenAI API compatibility
    json_keys_camel_to_snake(&mut req_json);

    // Fill default model if missing or empty
    if let Some(obj) = req_json.as_object_mut() {
        let model_val = obj
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(get_llm_model);

        obj.insert("model".to_string(), serde_json::Value::String(model_val));
    }

    // Merge default extra_body with per-request extra_body.
    // Client default extra_body serves as base, per-request fields override.
    if let Some(default_body) = client.extra_body.as_object() {
        let mut merged = default_body.clone();
        if let Some(req_body) = req_json
            .as_object()
            .and_then(|o| o.get("extra_body"))
            .and_then(|v| v.as_object())
        {
            for (k, v) in req_body.iter() {
                merged.insert(k.clone(), v.clone());
            }
        }

        if let Some(obj) = req_json.as_object_mut() {
            obj.insert("extra_body".to_string(), serde_json::Value::Object(merged));
        }
    }

    // Use reqwest to make HTTP request, build client
    let mut req_builder = reqwest::Client::new()
        .post(&format!("{}/chat/completions", client.base_url))
        .body(serde_json::to_string(&req_json).unwrap_or_default())
        .header("Content-Type", "application/json");

    if !client.api_key.is_empty() {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", client.api_key));
    }
    if let Some(dur) = client.timeout {
        req_builder = req_builder.timeout(dur);
    }

    if is_stream {
        // Streaming: AsyncIterableIterator
        let result_slot = PromiseResultSlot::<ChatReply>::new();
        let slot = result_slot.clone();
        let buffered_tool_call = client.buffered_tool_call;

        tokio::spawn(async move {
            async fn flush_buf(
                slot: &PromiseResultSlot<ChatReply>,
                buf: &mut Option<ToolCallBuffer>,
            ) {
                if let Some(state) = buf.take() {
                    slot.commit_async(ChatReply::Chunk(state.into_chunk()))
                        .await;
                }
            }

            match req_builder.send().await {
                Ok(response) => {
                    // Check HTTP status before reading SSE stream.
                    // non-2xx bodies are plain JSON, not SSE — they would be
                    // silently swallowed by the "data: " prefix filter otherwise.
                    let status = response.status();
                    if !status.is_success() {
                        let body = response.text().await.unwrap_or_default();
                        // Try to extract error message from OpenAI-style JSON body
                        let msg = serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|j| j["error"]["message"].as_str().map(String::from))
                            .unwrap_or_else(|| format!("HTTP {} : {}", status, body));
                        slot.commit_async(ChatReply::Error(msg)).await;
                        return;
                    }

                    // parse SSE manually
                    let byte_stream = response
                        .bytes_stream()
                        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));

                    let reader = StreamReader::new(byte_stream);
                    let mut lines = tokio::io::BufReader::new(reader).lines();
                    let mut buf: Option<ToolCallBuffer> = None;

                    while let Ok(Some(line)) = lines.next_line().await {
                        let line = line.trim().to_string();

                        if line.starts_with("data: ") {
                            let data = &line[6..];
                            if data == "[DONE]" {
                                if buf.is_some() {
                                    flush_buf(&slot, &mut buf).await;
                                }
                                break;
                            }

                            if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                if json.get("error").is_some() {
                                    if buf.is_some() {
                                        flush_buf(&slot, &mut buf).await;
                                    }

                                    let err_msg = json["error"]["message"]
                                        .as_str()
                                        .unwrap_or(data)
                                        .to_string();
                                    slot.commit_async(ChatReply::Error(err_msg)).await;
                                    break;
                                }

                                if buffered_tool_call {
                                    // Check for tool_calls in this chunk
                                    if let Some(tcs) =
                                        json["choices"][0]["delta"]["tool_calls"].as_array()
                                        && !tcs.is_empty()
                                    {
                                        // Accumulate tool_call deltas
                                        for tc in tcs {
                                            let idx = tc["index"].as_i64().unwrap_or(0) as usize;
                                            // Got a new index → flush previous buffered tool_call (matches openai-node logic)
                                            if let Some(ref cur) = buf
                                                && cur.idx != idx
                                            {
                                                flush_buf(&slot, &mut buf).await;
                                            }

                                            let state =
                                                buf.get_or_insert_with(|| ToolCallBuffer::new(idx));
                                            state.apply_delta(tc, &json);
                                        }

                                        // check finish_reason AFTER processing tool_calls
                                        // (non-standard servers may emit both in one chunk)
                                        if json["choices"][0]["finish_reason"].as_str().is_some() {
                                            flush_buf(&slot, &mut buf).await;

                                            // MUST NOT emit the original json as-is: it still carries the last
                                            // tool_calls delta. Emit a clean finish-only chunk instead.
                                            let mut finish = json.clone();
                                            if let Some(obj) =
                                                finish["choices"][0]["delta"].as_object_mut()
                                            {
                                                obj.remove("tool_calls");
                                            }
                                            slot.commit_async(ChatReply::Chunk(finish)).await;
                                        }
                                    } else {
                                        // Non-tool_call (content, finish_reason, etc.) → flush then passthrough
                                        flush_buf(&slot, &mut buf).await;
                                        slot.commit_async(ChatReply::Chunk(json)).await;
                                    }
                                } else {
                                    slot.commit_async(ChatReply::Chunk(json)).await;
                                }
                            }
                        }
                    }

                    slot.commit_async(ChatReply::Done).await;
                }
                Err(e) => {
                    slot.commit_async(ChatReply::Error(e.to_string())).await;
                }
            }
        });

        AsyncIterableIterator::create_with_result_slot(
            ctx,
            result_slot,
            |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    ChatReply::Chunk(chunk) => JSValue::from_serde_json(cx, &chunk).map(Some),
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

        tokio::spawn(async move {
            let result = match req_builder.send().await {
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

/// Getter magic constants
const PROP_MODEL: i32 = 0;

fn js_client_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, this)?;

    match cc.magic() {
        PROP_MODEL => match &client.model_name {
            Some(name) => ctx.create_string_from_str(name).map(JSValue::String),
            None => Ok(JSValue::NULL),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

fn js_client_dispose(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let _ = cc
        .this()
        .as_native_object_mut::<JSLlmClient>()
        .and_then(|ob| ob.take_payload());

    Ok(JSValue::UNDEFINED)
}

const CLASS_CLIENT_PROTO: &[FieldDef] = &[
    FieldDef::FUNC(
        "chat",
        1,
        js_client_chatCompletions,
        MAGIC_CHAT_STREAM as i16,
    ),
    FieldDef::FUNC(
        "chatOnce",
        1,
        js_client_chatCompletions,
        MAGIC_CHAT_ONCE as i16,
    ),
    FieldDef::FUNC(
        "responses",
        1,
        js_client_responses,
        MAGIC_RESPONSES_STREAM as i16,
    ),
    FieldDef::FUNC(
        "responsesOnce",
        1,
        js_client_responses,
        MAGIC_RESPONSES_ONCE as i16,
    ),
    FieldDef::FUNC("dispose", 0, js_client_dispose, 0),
    FieldDef::GETSET("model", Some(js_client_getter), None, PROP_MODEL as _),
];

def_native_object!(
    "Client",
    JSLlmClient,
    Arc<ClientPayload>,
    CLASS_CLIENT_PROTO,
    constructor: ClassCtorDef::new(js_client_constructor, 1, false)
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_openai(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlmClient::register(rt)
}
