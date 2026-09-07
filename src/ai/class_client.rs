// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use linkme::distributed_slice;
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
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{USER_CLASS_DEFS, helpers::json_keys_camel_to_snake};

use super::{
    MAGIC_CHAT_ONCE, MAGIC_CHAT_STREAM, MAGIC_RESPONSES_ONCE, MAGIC_RESPONSES_STREAM,
    OUTMSG_FIELD_CHOICES, OUTMSG_FIELD_REASONING, class_client_response::js_client_responses,
};

#[derive(Debug)]
pub(crate) struct ClientPayload {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: std::collections::HashMap<String, String>,
    pub(crate) timeout: Duration,

    //
    // model params
    //
    pub(crate) model_name: String,
    pub(crate) max_tokens: usize,
    pub(crate) extra_body: serde_json::Value,
}

impl GcTrace for ClientPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

pub(super) fn get_client<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a ClientPayload, i32> {
    JSLlmClient::payload_of(val).ok_or_else(|| ctx.throw_type_error("not an LlmClient instance"))
}

pub(super) fn get_client_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut ClientPayload, i32> {
    JSLlmClient::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not an LlmClient instance"))
}

/// Constructor: new LlmClient({apiKey, baseUrl, model?, timeout?})
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
    let mut timeout = 600; // sec
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
    let mut headers = std::collections::HashMap::new();
    let new_target = cc.this();

    let v = cc.arg(0);
    if !v.is_undefined() {
        let opts_obj = v
            .as_object()
            .ok_or_else(|| ctx.throw_type_error("requires options object"))?;

        let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
        if !base_url_val.is_undefined()
            && let Some(s) = base_url_val.as_string(true)
        {
            base_url = s.content().encode_utf8(false).to_string()
        }

        // apiKey
        let key_val = opts_obj.get_property_str(ctx, "apiKey")?;
        if let Some(s) = key_val
            .as_string(true)
            .map(|s| s.content().encode_utf8(false).to_string())
        {
            api_key = s
        }

        let model_val = opts_obj.get_property_str(ctx, "model")?;
        if !model_val.is_undefined()
            && let Some(s) = model_val
                .as_string(true)
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
                return Err(ctx.throw_type_error("invalid maxTokens"));
            }
        }

        let timeout_val = opts_obj.get_property_str(ctx, "timeout")?;
        if !timeout_val.is_undefined() {
            let n = timeout_val.to_number(ctx).map(|n| n.to_i32())?;
            if n >= 0 {
                timeout = n;
            }
        }

        // extra_body
        let v = opts_obj.get_property_str(ctx, "extraBody")?;
        if let Some(o) = v.as_object() {
            extra_body = o.to_serde_json(ctx)?;
        } else if v.is_null() {
            extra_body = serde_json::Value::Null;
        } else if !v.is_undefined() {
            return Err(ctx.throw_type_error("extraBody must be one of undefined, null or object"));
        }

        debug_assert!(extra_body.is_null() || extra_body.is_object());

        // headers
        let headers_val = opts_obj.get_property_str(ctx, "headers")?;
        if let Some(mut obj) = headers_val.as_object() {
            for (pk, val) in obj.get_own_property_kvs(
                ctx,
                zuqe::JS_GPN_STR_OR_INT_MASK,
                zuqe::JSIteratorKindEnum::JS_ITERATOR_KIND_KEY_AND_VALUE,
            )? {
                if let Some(key) = pk.as_string(true)
                    && let Some(val_str) = val.as_string(true)
                {
                    headers.insert(
                        key.content().encode_utf8(false).to_string(),
                        val_str.content().encode_utf8(false).to_string(),
                    );
                }
            }
        } else if !headers_val.is_undefined_or_null() {
            return Err(ctx.throw_type_error("headers must be an object"));
        }
    }

    let payload = ClientPayload {
        model_name: model,
        base_url,
        api_key,
        timeout: Duration::from_secs(timeout as u64),
        max_tokens: max_tokens as usize,
        extra_body,
        headers,
    };

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
    let client = get_client(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;

    let opts_obj = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("chat() requires an options object"))?;

    // Convert JS options to raw JSON, preserving all fields (including extension
    // fields like reasoning_content) that async-openai typed structs would drop.
    // LlmClient is a remote API proxy — it should forward everything, not filter.
    let mut req_json = opts_obj.to_serde_json(ctx)?;

    // Extract per-request timeout (seconds), defaults to client-level timeout
    // This field is NOT sent to the API — it controls HTTP timeout only
    let req_timeout_secs = req_json
        .as_object()
        .and_then(|obj| obj.get("timeout")?.as_f64())
        .map(|n| n as u64);
    if let Some(obj) = req_json.as_object_mut() {
        obj.remove("timeout");
    }

    // Per-request stream-idle timeout (seconds): abort when no SSE data has
    // arrived for this long. Also NOT sent to the API.
    let idle_timeout_secs = req_json
        .as_object()
        .and_then(|obj| obj.get("idle_timeout")?.as_f64())
        .map(|n| n as u64);
    if let Some(obj) = req_json.as_object_mut() {
        obj.remove("idle_timeout");
    }

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
                    // LlmClient default extra_body serves as base, per-request fields override.
                    for (k, v) in req_body.iter() {
                        merged.insert(k.clone(), v.clone());
                    }
                    req.insert("extra_body".to_string(), serde_json::Value::Object(merged));
                }
            } else {
                return Err(ctx.throw_type_error("expect object: extraBody"));
            }
        } else if client.extra_body.is_object() {
            req.insert("extra_body".to_string(), client.extra_body.clone());
        }

        // Always tell the server explicitly whether to stream.
        req.insert("stream".into(), serde_json::Value::Bool(is_stream));

        if is_stream && !req.contains_key("stream_options") {
            let mut so = serde_json::Map::new();
            so.insert("include_usage".into(), serde_json::Value::Bool(true));
            req.insert("stream_options".into(), serde_json::Value::Object(so));
        }
    }

    // Use reqwest to make HTTP request, build client
    let mut req_builder = reqwest::Client::new()
        .post(format!("{}/chat/completions", client.base_url))
        .body(serde_json::to_string(&req_json).unwrap_or_default())
        .header("Content-Type", "application/json")
        .timeout(client.timeout); // client default timeout

    // Apply custom headers (overrides Authorization if same key)
    for (key, value) in &client.headers {
        req_builder = req_builder.header(key.as_str(), value.as_str());
    }

    if !client.api_key.is_empty() {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", client.api_key));
    }

    // Per-request timeout
    if let Some(secs) = req_timeout_secs {
        req_builder = req_builder.timeout(Duration::from_secs(secs));
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

                    // Idle guard: wrap each frame read with a per-call timeout.
                    // tokio::time::timeout measures the gap between consecutive
                    // SSE lines, so any received byte naturally resets it. When
                    // it fires, an Error is committed and the buffered
                    // tool-call fragment is intentionally dropped (stalled
                    // partial output is worthless).
                    let mut idle_aborted = false;
                    let idle_limit = idle_timeout_secs.map(|s| std::time::Duration::from_secs(s));

                    'sse: loop {
                        let next = match idle_limit {
                            Some(idle) => {
                                match tokio::time::timeout(idle, lines.next_line()).await {
                                    Ok(inner) => inner,
                                    Err(_) => {
                                        idle_aborted = true;
                                        slot.commit_async(ChatReply::Error(format!(
                                            "llm stream idle timeout: no data for {}s",
                                            idle.as_secs()
                                        )))
                                        .await;
                                        break 'sse;
                                    }
                                }
                            }
                            None => lines.next_line().await,
                        };
                        // Preserve legacy end semantics: Ok(Some)=frame,
                        // Ok(None)=EOF, Err=end silently.
                        let Ok(Some(line)) = next else {
                            break 'sse;
                        };
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

                                // Check for tool_calls in this chunk
                                if let Some(tcs) =
                                    json[OUTMSG_FIELD_CHOICES][0]["delta"]["tool_calls"].as_array()
                                    && !tcs.is_empty()
                                {
                                    // A chunk may carry content/reasoning alongside tool_calls.
                                    // Since we only buffer tool_calls here, emit that text now —
                                    // otherwise it is lost when finish_reason arrives in a later,
                                    // content-less chunk.
                                    let has_finish = json[OUTMSG_FIELD_CHOICES][0]["finish_reason"]
                                        .as_str()
                                        .is_some();
                                    let delta = &json[OUTMSG_FIELD_CHOICES][0]["delta"];
                                    if !has_finish
                                        && (delta.get("content").is_some()
                                            || delta.get("reasoning_content").is_some())
                                    {
                                        let mut text_chunk = json.clone();
                                        if let Some(obj) =
                                            text_chunk[OUTMSG_FIELD_CHOICES][0]["delta"]
                                                .as_object_mut()
                                        {
                                            obj.remove("tool_calls");
                                        }
                                        if !has_normalizer {
                                            normalize_message_shape(&mut text_chunk);
                                        }
                                        slot.commit_async(ChatReply::Chunk(text_chunk)).await;
                                    }

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
                                            finish[OUTMSG_FIELD_CHOICES][0]["delta"].as_object_mut()
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
                            }
                        }
                    }

                    // Stream ended without [DONE] (server closed early): flush
                    // any still-buffered tool_call so it is not silently lost.
                    // On the normal [DONE] path the buffer is already empty.
                    // Skipped when the idle guard aborted the stream.
                    if !idle_aborted {
                        if buf.is_some() {
                            flush_buf(&slot, &mut buf).await;
                        }
                        slot.commit_async(ChatReply::Done).await;
                    }
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
                    ChatReply::Error(e) => Err(cx.throw_internal_error(&e)),
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
                    Err(e) => Err(cx.throw_internal_error(&e)),
                }
            },
        )?;

        tokio::spawn(async move {
            let result = match req_builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if status.is_success() {
                        if body.is_empty() {
                            // HTTP success, but empty response body -> upstream failure
                            Err(format!("HTTP {} but returns empty body", status))
                        } else {
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
                                Err(e) => {
                                    let snippet = if body.len() > 200 {
                                        &body[..200]
                                    } else {
                                        &body
                                    };
                                    Err(format!("Failed to parse JSON: {} | body: {}", e, snippet))
                                }
                            }
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
const PROP_EXTRA_BODY: i32 = 4;
const PROP_TIMEOUT: i32 = 5;
const PROP_MAX_TOKENS: i32 = 6;
const PROP_HEADERS: i32 = 7;

/// Internal property key for responseNormalizer, hidden from JS enumeration
const INNER_PROP_RESPONSE_NORMALIZER: &str = "_responseNormalizer";

fn js_client_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, &this)?;

    match cc.magic() {
        PROP_MODEL => ctx
            .create_string_from_str(&client.model_name)
            .map(JSValue::make_string),
        PROP_BASE_URL => ctx
            .create_string_from_str(&client.base_url)
            .map(JSValue::make_string),
        PROP_MAX_TOKENS => Ok(JSValue::from(client.max_tokens as i32)),
        PROP_EXTRA_BODY => {
            if client.extra_body.is_null() {
                Ok(JSValue::NULL)
            } else {
                JSValue::from_serde_json(ctx, &client.extra_body)
            }
        }
        PROP_TIMEOUT => Ok(JSValue::from(client.timeout.as_secs() as i32)),
        PROP_HEADERS => {
            let mut obj = ctx.create_base_object()?;
            for (k, v) in &client.headers {
                let val = ctx.create_string_from_str(v)?;
                obj.set_property_str(ctx, k, JSValue::make_string(val))?;
            }
            Ok(JSValue::from(obj))
        }
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
    let this = cc.this();
    let val = cc.arg(0);
    if !val.is_null() && !val.is_callable() {
        return Err(ctx.throw_internal_error("responseNormalizer must be a function or null"));
    }
    if let Some(mut obj) = this.as_object() {
        obj.set_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER, val)?;
    }
    Ok(JSValue::UNDEFINED)
}

fn js_client_set_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let name_val = cc.arg(0);
    let val = cc.arg(1);

    let name = match name_val.as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => {
            return Err(ctx.throw_type_error("setHeader: header name must be a string"));
        }
    };

    let payload = get_client_mut(ctx, &mut this)?;

    // Undefined → no-op (matches the bulk setter's no-op semantics).
    if val.is_undefined() {
        return Ok(JSValue::UNDEFINED);
    }
    // Null → delete the entry.
    if val.is_null() {
        payload.headers.remove(&name);
        return Ok(JSValue::UNDEFINED);
    }
    // String → insert/update.
    let s = match val.as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => {
            return Err(
                ctx.throw_type_error("setHeader: value must be a string, null, or undefined")
            );
        }
    };
    payload.headers.insert(name, s);
    Ok(JSValue::UNDEFINED)
}

fn js_client_get_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let name_val = cc.arg(0);

    let name = match name_val.as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => {
            return Err(ctx.throw_type_error("getHeader: header name must be a string"));
        }
    };

    let payload = get_client(ctx, &this)?;
    match payload.headers.get(&name) {
        Some(v) => ctx.create_string_from_str(v).map(JSValue::make_string),
        None => Ok(JSValue::UNDEFINED),
    }
}

fn js_client_dispose(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let _ = JSLlmClient::take_payload_of(&mut this);

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
    FieldDef::FUNC("setHeader", 2, js_client_set_header, 0),
    FieldDef::FUNC("getHeader", 1, js_client_get_header, 0),
    FieldDef::GETSET("model", Some(js_client_getter), None, PROP_MODEL as _),
    FieldDef::GETSET("baseUrl", Some(js_client_getter), None, PROP_BASE_URL as _),
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
    FieldDef::GETSET("headers", Some(js_client_getter), None, PROP_HEADERS as _),
    FieldDef::GETSET(
        "responseNormalizer",
        Some(js_client_get_normalizer),
        Some(js_client_set_normalizer),
        PROP_RESPONSE_NORMALIZER as _,
    ),
];

def_native_object!(
    "LlmClient",
    JSLlmClient,
    ClientPayload,
    CLASS_CLIENT_PROTO,
    Some(ClassCtorDef::new(js_client_constructor, 1, false))
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_openai(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSLlmClient::register(rt)
}
