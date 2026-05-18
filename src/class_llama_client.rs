// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, sync::Arc};

use futures_util::StreamExt;
use linkme::distributed_slice;
use reqwest::header::HeaderMap;
use tokio::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;
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
        ctx.throw_type_error("requires options object");
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

    let default_model: Option<String> = opts_obj
        .get_property_str(ctx, "model")?
        .as_string()
        .map(|s| s.content().encode_utf8(false).to_string());

    let buffered_tool_call = true;
    // let buffered_tool_call = opts_obj
    //     .get_property_str(ctx, "bufferedToolCall")?
    //     .as_bool()
    //     .unwrap_or(false);

    JSLlamaClient::new_instance(
        ctx,
        default_model,
        base_url,
        api_key,
        String::new(),
        String::new(),
        HeaderMap::new(),
        buffered_tool_call,
    )
    .map(|obj| obj.into())
}

pub(crate) struct ClientState {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) org_id: String,
    pub(crate) project_id: String,
    pub(crate) custom_headers: HeaderMap,
    pub(crate) model_name: Option<String>,
    pub(crate) buffered_tool_call: bool,
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

    // Use reqwest to make HTTP request, build client
    let http_client = reqwest::Client::new();
    let mut client = http_client
        .post(&format!("{}/chat/completions", client_arc.base_url))
        .body(serde_json::to_string(&req).unwrap_or_default())
        .header("Content-Type", "application/json");

    if !client_arc.api_key.is_empty() {
        client = client.header("Authorization", format!("Bearer {}", client_arc.api_key));
    }

    if is_stream {
        // Streaming: AsyncIterableIterator
        let result_slot = PromiseResultSlot::<ChatReply>::new();
        let slot = result_slot.clone();
        let buffered_tool_call = client_arc.buffered_tool_call;

        tokio::spawn(async move {
            async fn commit_slot(slot: &PromiseResultSlot<ChatReply>, mut msg: ChatReply) {
                loop {
                    match slot.commit(msg) {
                        Ok(()) => {
                            return;
                        }
                        Err(ret) => {
                            // slot is full, yield and do commit later
                            msg = ret;
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }

            async fn flush_buf(
                slot: &PromiseResultSlot<ChatReply>,
                buf: &mut Option<ToolCallBuffer>,
            ) {
                if let Some(state) = buf.take() {
                    commit_slot(slot, ChatReply::Chunk(state.into_chunk())).await;
                }
            }

            match client.send().await {
                Ok(response) => {
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
                                    commit_slot(&slot, ChatReply::Error(err_msg)).await;
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
                                            commit_slot(&slot, ChatReply::Chunk(finish)).await;
                                        }
                                    } else {
                                        // Non-tool_call (content, finish_reason, etc.) → flush then passthrough
                                        flush_buf(&slot, &mut buf).await;
                                        commit_slot(&slot, ChatReply::Chunk(json)).await;
                                    }
                                } else {
                                    commit_slot(&slot, ChatReply::Chunk(json)).await;
                                }
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
            let result = match client.send().await {
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
    let client_arc = get_client_arc(ctx, &this)?;

    match cc.magic() {
        PROP_MODEL => match &client_arc.model_name {
            Some(name) => ctx.create_string_from_str(name).map(JSValue::String),
            None => Ok(JSValue::NULL),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

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
    FieldDef::GETSET("model", Some(js_client_getter), None, PROP_MODEL as _),
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
        model: Option<String>,
        base_url: String,
        api_key: String,
        org_id: String,
        project_id: String,
        custom_headers: HeaderMap,
        buffered_tool_call: bool,
    ) -> Result<Object, i32> {
        let state = Arc::new(ClientState {
            model_name: model,
            base_url,
            api_key,
            org_id,
            project_id,
            custom_headers,
            buffered_tool_call,
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
