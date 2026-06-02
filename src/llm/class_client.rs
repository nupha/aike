// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, sync::Arc, time::Duration};

use futures_util::StreamExt;
use linkme::distributed_slice;
use reqwest::header::HeaderMap;
use tokio::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JS_PROP_WRITABLE, JSResult, JSValue, Runtime,
    ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
    def_native_object,
};

use crate::{USER_CLASS_DEFS, helpers::json_keys_camel_to_snake};

use super::{
    OUTMSG_FIELD_CHOICES, OUTMSG_FIELD_REASONING,
    class_client_response::{MAGIC_RESPONSES_ONCE, MAGIC_RESPONSES_STREAM, js_client_responses},
    class_engine::{MAGIC_CHAT_ONCE, MAGIC_CHAT_STREAM},
};

#[derive(Debug)]
pub(crate) struct ClientPayload {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) org_id: String,
    pub(crate) project_id: String,
    pub(crate) buffered_tool_call: bool,
    pub(crate) timeout: Option<Duration>,
    pub(crate) custom_headers: HeaderMap,

    //
    // default params
    //
    pub(crate) model_name: String,
    pub(crate) max_tokens: usize,
    pub(crate) extra_body: serde_json::Value,
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
    // params from envvar
    let mut base_url = std::env::var("AIKE_LLM_BASE_URL").unwrap_or_default();
    let mut api_key = std::env::var("AIKE_LLM_API_KEY").unwrap_or_default();
    let mut model = std::env::var("AIKE_LLM_MODEL").unwrap_or_default();
    let mut max_tokens = if let Ok(s) = std::env::var("AIKE_LLM_MAX_TOKENS")
        && !s.is_empty()
    {
        if let Ok(n) = s.parse::<i32>()
            && n >= 0
        {
            n
        } else {
            0
        }
    } else {
        0
    };
    let mut timeout_ms = 0;
    let mut extra_body = {
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
    };
    let mut buffered_tool_call = true;

    let new_target = cc.this();

    let v = cc.arg(0);
    if !v.is_undefined() {
        let opts_obj = v.as_object().copied().ok_or_else(|| {
            ctx.throw_type_error("requires options object");
            -1
        })?;

        let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
        if !base_url_val.is_undefined()
            && let Some(s) = base_url_val.as_string()
        {
            base_url = s.content().encode_utf8(false).to_string()
        }

        let key_val = opts_obj.get_property_str(ctx, "apiKey")?;
        if !key_val.is_undefined()
            && let Some(s) = key_val
                .as_string()
                .map(|s| s.content().encode_utf8(false).to_string())
        {
            api_key = s
        }

        let model_val = opts_obj.get_property_str(ctx, "model")?;
        if !model_val.is_undefined()
            && let Some(s) = model_val
                .as_string()
                .map(|s| s.content().encode_utf8(false).to_string())
        {
            model = s;
        }

        let tokens_val = opts_obj.get_property_str(ctx, "maxTokens")?;
        if !tokens_val.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                max_tokens = n;
            } else {
                ctx.throw_type_error("invalid maxTokens");
                return Err(-1);
            }
        }

        let timeout_val = opts_obj.get_property_str(ctx, "timeout")?;
        if !timeout_val.is_undefined() {
            let n = timeout_val.to_number(ctx).map(|n| n.to_i32())?;
            if n >= 0 {
                timeout_ms = n;
            }
        }

        // extra_body
        match opts_obj.get_property_str(ctx, "extraBody")? {
            JSValue::UNDEFINED => {}
            JSValue::NULL => {
                extra_body = serde_json::Value::Null;
            }
            JSValue::Object(o) => {
                extra_body = o.to_serde_json(ctx)?;
            }
            _ => {
                ctx.throw_type_error("extraBody must be one of undefined, null or object");
                return Err(-1);
            }
        };
        debug_assert!(extra_body.is_null() || extra_body.is_object());
    }

    let payload = Arc::new(ClientPayload {
        model_name: model,
        base_url,
        api_key,
        org_id: String::new(),
        project_id: String::new(),
        custom_headers: HeaderMap::new(),
        buffered_tool_call,
        timeout: if timeout_ms > 0 {
            Some(Duration::from_millis(timeout_ms as u64))
        } else {
            None
        },
        max_tokens: max_tokens as usize,
        extra_body,
    });

    JSLlmClient::new_instance(ctx, new_target, payload).and_then(|mut o| {
        // Initialize hidden internal property for responseNormalizer,
        // pre-created so the setter can use simple set_property_str.
        o.as_object_mut().define_property_value_str(
            ctx,
            INNER_PROP_RESPONSE_NORMALIZER,
            JSValue::NULL,
            JS_PROP_WRITABLE,
        )?;

        Ok(JSValue::from(o))
    })
}

/// Normalize message format in `choices[]` to standard shape.
/// For both streaming delta and non-streaming message.
fn normalize_message_shape(obj: &mut serde_json::Value) {
    if let Some(choices) = obj[OUTMSG_FIELD_CHOICES].as_array_mut() {
        for choice in choices.iter_mut() {
            // The target is either ‘delta’ (streaming) or ‘message’ (non-streaming).
            let target = match choice.as_object_mut() {
                Some(m) => {
                    if let Some(v) = m.get_mut("delta") {
                        v.as_object_mut()
                    } else if let Some(v) = m.get_mut("message") {
                        v.as_object_mut()
                    } else {
                        None
                    }
                }
                None => None,
            };

            if let Some(target) = target {
                // 1. fix reasoning → reasoning_content
                if let Some(val) = target.remove("reasoning")
                    && !target.contains_key(OUTMSG_FIELD_REASONING)
                {
                    target.insert(OUTMSG_FIELD_REASONING.to_string(), val);
                }
            }
        }
    }
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

    if let Some(req) = req_json.as_object_mut() {
        // model
        if req.get("model").is_none_or(|v| !v.is_string()) {
            req.insert(
                "model".into(),
                serde_json::Value::String(client.model_name.to_string()),
            );
        }

        // max_tokens
        if req.get("max_tokens").is_none() && client.max_tokens > 0 {
            req.insert("max_tokens".into(), client.max_tokens.into());
        }

        // extra_body
        if let Some(b) = req.get("extra_body") {
            if let Some(req_body) = b.as_object() {
                if let Some(mut merged) = client.extra_body.as_object().cloned() {
                    // Merge default extra_body with per-request extra_body.
                    // Client default extra_body serves as base, per-request fields override.
                    for (k, v) in req_body.iter() {
                        merged.insert(k.clone(), v.clone());
                    }
                    req.insert("extra_body".to_string(), serde_json::Value::Object(merged));
                }
            } else {
                ctx.throw_type_error("expect object: extraBody");
                return Err(-1);
            }
        } else if client.extra_body.is_object() {
            req.insert("extra_body".to_string(), client.extra_body.clone());
        }
    }

    // Use reqwest to make HTTP request, build client
    let mut req_builder = reqwest::Client::new()
        .post(format!("{}/chat/completions", client.base_url))
        .body(serde_json::to_string(&req_json).unwrap_or_default())
        .header("Content-Type", "application/json");

    if !client.api_key.is_empty() {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", client.api_key));
    }
    if let Some(dur) = client.timeout {
        req_builder = req_builder.timeout(dur);
    }

    // Read custom response normalizer from instance property
    let normalizer = if let Some(obj) = this.as_object() {
        obj.get_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER)?
    } else {
        JSValue::UNDEFINED
    };
    let has_normalizer = normalizer.is_callable();

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
                        .map(|r| r.map_err(std::io::Error::other));

                    let reader = StreamReader::new(byte_stream);
                    let mut lines = tokio::io::BufReader::new(reader).lines();
                    let mut buf: Option<ToolCallBuffer> = None;

                    while let Ok(Some(line)) = lines.next_line().await {
                        let line = line.trim().to_string();

                        if let Some(data) = line.strip_prefix("data: ") {
                            if data == "[DONE]" {
                                if buf.is_some() {
                                    flush_buf(&slot, &mut buf).await;
                                }
                                break;
                            }

                            if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(data) {
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
                                        json[OUTMSG_FIELD_CHOICES][0]["delta"]["tool_calls"]
                                            .as_array()
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
                                        if json[OUTMSG_FIELD_CHOICES][0]["finish_reason"]
                                            .as_str()
                                            .is_some()
                                        {
                                            flush_buf(&slot, &mut buf).await;

                                            // MUST NOT emit the original json as-is: it still carries the last
                                            // tool_calls delta. Emit a clean finish-only chunk instead.
                                            let mut finish = json.clone();
                                            if let Some(obj) =
                                                finish[OUTMSG_FIELD_CHOICES][0]["delta"]
                                                    .as_object_mut()
                                            {
                                                obj.remove("tool_calls");
                                            }
                                            // Normalize message format in choices[] delta
                                            if !has_normalizer {
                                                normalize_message_shape(&mut finish);
                                            }
                                            slot.commit_async(ChatReply::Chunk(finish)).await;
                                        }
                                    } else {
                                        // Non-tool_call (content, finish_reason, etc.) → flush then passthrough
                                        flush_buf(&slot, &mut buf).await;
                                        // Normalize message format in choices[] delta
                                        if !has_normalizer {
                                            normalize_message_shape(&mut json);
                                        }
                                        slot.commit_async(ChatReply::Chunk(json)).await;
                                    }
                                } else {
                                    // Normalize message format in choices[] delta
                                    if !has_normalizer {
                                        normalize_message_shape(&mut json);
                                    }
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
            move |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    ChatReply::Chunk(chunk) => {
                        let val = JSValue::from_serde_json(cx, &chunk)?;
                        let val = if normalizer.is_callable() {
                            normalizer.call(cx, JSValue::UNDEFINED, &[val])?
                        } else {
                            val
                        };
                        Ok(Some(val))
                    }
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
        let normalizer_ns = normalizer;
        let promise = Promise::create_with_result_slot(
            ctx,
            result_slot.clone(),
            move |cx, data| -> JSResult {
                match data {
                    Ok(resp) => {
                        let val = JSValue::from_serde_json(cx, &resp)?;
                        let val = if normalizer_ns.is_callable() {
                            normalizer_ns.call(cx, JSValue::UNDEFINED, &[val])?
                        } else {
                            val
                        };
                        Ok(val)
                    }
                    Err(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                }
            },
        )?;

        tokio::spawn(async move {
            let result = match req_builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if status.is_success() {
                        match serde_json::from_str::<serde_json::Value>(&body) {
                            Ok(mut json) => {
                                // Check for error in response
                                if json.get("error").is_some() {
                                    let err_msg = json["error"]["message"]
                                        .as_str()
                                        .unwrap_or(&body)
                                        .to_string();
                                    Err(err_msg)
                                } else {
                                    // Normalize message format in choices[] message
                                    if !has_normalizer {
                                        normalize_message_shape(&mut json);
                                    }
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
const PROP_RESPONSE_NORMALIZER: i32 = 1;
const PROP_BASE_URL: i32 = 2;
const PROP_BUFFERED_TOOL_CALL: i32 = 3;
const PROP_EXTRA_BODY: i32 = 4;
const PROP_TIMEOUT: i32 = 5;
const PROP_MAX_TOKENS: i32 = 6;

/// Internal property key for responseNormalizer, hidden from JS enumeration
const INNER_PROP_RESPONSE_NORMALIZER: &str = "_responseNormalizer";

fn js_client_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, this)?;

    match cc.magic() {
        PROP_MODEL => ctx
            .create_string_from_str(&client.model_name)
            .map(JSValue::String),
        PROP_BASE_URL => ctx
            .create_string_from_str(&client.base_url)
            .map(JSValue::String),
        PROP_BUFFERED_TOOL_CALL => Ok(JSValue::Bool(client.buffered_tool_call)),
        PROP_MAX_TOKENS => Ok(JSValue::Int(client.max_tokens as i32)),
        PROP_EXTRA_BODY => {
            if client.extra_body.is_null() {
                Ok(JSValue::NULL)
            } else {
                JSValue::from_serde_json(ctx, &client.extra_body)
            }
        }
        PROP_TIMEOUT => match client.timeout {
            Some(dur) => Ok(JSValue::from(dur.as_millis() as f64)),
            None => Ok(JSValue::NULL),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

fn js_client_get_normalizer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let val = if let Some(obj) = this.as_object() {
        obj.get_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER)?
    } else {
        JSValue::UNDEFINED
    };
    Ok(val)
}

fn js_client_set_normalizer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let val = cc.arg(0);
    if !val.is_null() && !val.is_callable() {
        ctx.throw_internal_error("responseNormalizer must be a function or null");
        return Err(-1);
    }
    if let Some(obj) = this.as_object_mut() {
        obj.set_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER, val)?;
    }
    Ok(JSValue::UNDEFINED)
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
    FieldDef::GETSET("baseUrl", Some(js_client_getter), None, PROP_BASE_URL as _),
    FieldDef::GETSET(
        "bufferedToolCall",
        Some(js_client_getter),
        None,
        PROP_BUFFERED_TOOL_CALL as _,
    ),
    FieldDef::GETSET(
        "maxTokens",
        Some(js_client_getter),
        None,
        PROP_MAX_TOKENS as _,
    ),
    FieldDef::GETSET(
        "extraBody",
        Some(js_client_getter),
        None,
        PROP_EXTRA_BODY as _,
    ),
    FieldDef::GETSET("timeout", Some(js_client_getter), None, PROP_TIMEOUT as _),
    FieldDef::GETSET(
        "responseNormalizer",
        Some(js_client_get_normalizer),
        Some(js_client_set_normalizer),
        PROP_RESPONSE_NORMALIZER as _,
    ),
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
