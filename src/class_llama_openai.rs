// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, sync::Arc, time::Duration};

use linkme::distributed_slice;
use liter_llm::types::common::{
    AssistantMessage, Message, StopSequence, SystemMessage, UserContent, UserMessage,
};
use liter_llm::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ClientConfigBuilder,
    DefaultClient, LlmClient,
};
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassDef, ClassID, FieldDef, JSResult, JSValue, Runtime,
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
    ctx.create_class_instance_with_opaque(JSOpenAI::class_id(), opaque as _)
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
        .retrive_object_payload(ctx, JSOpenAI::class_id())?
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
            _ => {
                ctx.throw_type_error(&format!("unsupported role: {role}"));
                return Err(-1);
            }
        };
        messages.push(msg);
    }
    Ok(messages)
}

// ---------------------------------------------------------------------------
// Channel message type for streaming bridge
// ---------------------------------------------------------------------------

enum ChatReply {
    Chunk(ChatCompletionChunk),
    Done,
    Error(String),
}

/// chatCompletions(messages, {model?, stream?, temperature?, maxTokens?, topP?, stop?})
fn js_openai_chatCompletions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client_arc(ctx, &this)?;

    // Parse messages (arg 0)
    let messages = parse_messages(ctx, &cc.arg(0))?;

    // Parse options (arg 1)
    let opts_val = cc.arg(1);
    let opts = if opts_val.is_undefined() || opts_val.is_null() {
        None
    } else {
        Some(opts_val.as_object().copied().ok_or_else(|| {
            ctx.throw_type_error("options must be object");
            -1
        })?)
    };

    let mut model = String::from("default");
    let mut stream = false;
    let mut temperature: Option<f64> = None;
    let mut max_tokens: Option<u64> = None;
    let mut top_p: Option<f64> = None;
    let mut stop: Option<StopSequence> = None;

    if let Some(opts) = opts.as_ref() {
        let v = opts.get_property_str(ctx, "model")?;
        if let Some(s) = v.as_string() {
            model = s.content().encode_utf8(false).to_string();
        }

        let v = opts.get_property_str(ctx, "stream")?;
        if !v.is_undefined() {
            stream = v.to_bool()?;
        }

        let v = opts.get_property_str(ctx, "temperature")?;
        if !v.is_undefined() {
            temperature = Some(v.to_number(ctx)?.to_f64());
        }

        let v = opts.get_property_str(ctx, "maxTokens")?;
        if !v.is_undefined() {
            max_tokens = Some(v.to_number(ctx)?.to_f64() as u64);
        }

        let v = opts.get_property_str(ctx, "topP")?;
        if !v.is_undefined() {
            top_p = Some(v.to_number(ctx)?.to_f64());
        }

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
    }

    let req = ChatCompletionRequest {
        model,
        messages,
        temperature,
        max_tokens,
        top_p,
        stop,
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
        .retrive_object_payload_mut(ctx, JSOpenAI::class_id())?
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
    json_value_to_js(ctx, &val)
}

/// Convert ChatCompletionChunk to a JS object.
fn chunk_to_js(ctx: &mut ScopedContext, chunk: &ChatCompletionChunk) -> JSResult {
    let val = serde_json::to_value(chunk).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize chunk failed: {e}"));
        -1
    })?;
    json_value_to_js(ctx, &val)
}

/// Recursively convert a serde_json::Value to a JS value.
fn json_value_to_js(ctx: &mut ScopedContext, val: &serde_json::Value) -> Result<JSValue, i32> {
    match val {
        serde_json::Value::Null => Ok(JSValue::NULL),
        serde_json::Value::Bool(b) => Ok(JSValue::Bool(*b)),
        serde_json::Value::Number(n) => {
            // Prefer i32 for small integers, f64 otherwise
            if let Some(i) = n.as_i64() {
                if i >= i32::MIN as i64 && i <= i32::MAX as i64 {
                    Ok(JSValue::Int(i as i32))
                } else {
                    Ok((i as f64).into())
                }
            } else {
                Ok(n.as_f64().unwrap_or(0.0).into())
            }
        }
        serde_json::Value::String(s) => ctx
            .create_string_from_utf8(s.clone().into_bytes())
            .map(JSValue::String),
        serde_json::Value::Array(arr) => {
            let mut values = Vec::with_capacity(arr.len());
            for item in arr {
                values.push(json_value_to_js(ctx, item)?);
            }
            Array::from_values(ctx, values).map(JSValue::from)
        }
        serde_json::Value::Object(map) => {
            let mut obj = ctx.create_base_object()?;
            for (k, v) in map {
                let js_val = json_value_to_js(ctx, v)?;
                obj.set_property_str(ctx, k, js_val)?;
            }
            Ok(obj.into())
        }
    }
}

const CLASS_OPENAI_PROTO: &[FieldDef] = &[
    FieldDef::CALL("chatCompletions", 2, js_openai_chatCompletions, 0),
    FieldDef::CALL("dispose", 0, js_openai_dispose, 0),
];

pub struct JSOpenAI;

impl JSOpenAI {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt.register_class(
            ClassDef::new(
                "OpenAI",
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
    JSOpenAI::register(rt)
}
