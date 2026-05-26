// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use async_openai::types::responses::{
    CreateResponseArgs, EasyInputContent, EasyInputMessage, InputItem, InputParam, MessageType,
    Reasoning, Response, ResponseStreamEvent, Role,
};
use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
    },
};

use crate::get_llm_model;

use super::class_client::get_client;

pub(super) const MAGIC_RESPONSES_STREAM: i32 = 4;
pub(super) const MAGIC_RESPONSES_ONCE: i32 = 5;

enum ResponsesReply {
    Event(Box<ResponseStreamEvent>),
    Done,
    Error(String),
}

pub(super) fn parse_responses_input(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<InputParam, i32> {
    if let Some(s) = val.as_string() {
        return Ok(InputParam::Text(s.content().encode_utf8(false).to_string()));
    };

    if let Some(arr) = val.as_object_Array() {
        let mut items = Vec::with_capacity(arr.payload().len());

        for item in arr.payload().values() {
            let obj = item.as_object().copied().ok_or_else(|| {
                ctx.throw_type_error("input[] item must be object");
                -1
            })?;

            let role_val = obj.get_property_str(ctx, "role")?;
            let role_str = role_val
                .as_string()
                .map(|s| s.content().encode_utf8(false).to_string())
                .ok_or_else(|| {
                    ctx.throw_type_error("input[].role must be string");
                    -1
                })?;
            let role = match role_str.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                "developer" => Role::Developer,
                _ => {
                    ctx.throw_type_error(&format!("unsupported role: {role_str}"));
                    return Err(-1);
                }
            };

            let content_val = obj.get_property_str(ctx, "content")?;
            let content = if content_val.is_undefined_or_null() {
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

            items.push(InputItem::EasyMessage(EasyInputMessage {
                r#type: MessageType::Message,
                role,
                content: EasyInputContent::Text(content),
                phase: None,
            }));
        }

        return Ok(InputParam::Items(items));
    }

    ctx.throw_type_error("input must be a string or an array");
    Err(-1)
}

pub(super) fn parse_responses_reasoning(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Reasoning, i32> {
    let json_val = val.to_serde_json(ctx).map_err(|_| {
        ctx.throw_type_error("failed to serialize reasoning");
        -1
    })?;
    serde_json::from_value::<Reasoning>(json_val).map_err(|e| {
        ctx.throw_type_error(&format!("invalid reasoning: {e}"));
        -1
    })
}

fn response_to_js(ctx: &mut ScopedContext, resp: &Response) -> JSResult {
    let val = serde_json::to_value(resp).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize response failed: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

fn stream_event_to_js(ctx: &mut ScopedContext, event: &ResponseStreamEvent) -> JSResult {
    let val = serde_json::to_value(event).map_err(|e| {
        ctx.throw_internal_error(&format!("serialize stream event failed: {e}"));
        -1
    })?;
    JSValue::from_serde_json(ctx, &val)
}

pub(super) fn js_client_responses(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client_arc = get_client(ctx, this)?;
    let is_stream = cc.magic() == MAGIC_RESPONSES_STREAM;

    let opts = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("responses() requires an options object");
        -1
    })?;

    let input_val = opts.get_property_str(ctx, "input")?;
    let input = parse_responses_input(ctx, &input_val)?;

    let model_val = opts.get_property_str(ctx, "model")?;
    let model = if let Some(s) = model_val.as_string() {
        s.content().encode_utf8(false).to_string()
    } else {
        client_arc.model_name.clone().unwrap_or_else(get_llm_model)
    };

    let mut req_builder = CreateResponseArgs::default();
    req_builder.input(input).model(&model);

    let instructions_val = opts.get_property_str(ctx, "instructions")?;
    if let Some(s) = instructions_val.as_string() {
        req_builder.instructions(s.content().encode_utf8(false).to_string());
    }

    let temperature_val = opts.get_property_str(ctx, "temperature")?;
    if !temperature_val.is_undefined() {
        req_builder.temperature(temperature_val.to_number(ctx)?.to_f64() as f32);
    }

    let top_p_val = opts.get_property_str(ctx, "topP")?;
    if !top_p_val.is_undefined() {
        req_builder.top_p(top_p_val.to_number(ctx)?.to_f64() as f32);
    }

    let max_output_tokens_val = opts.get_property_str(ctx, "maxOutputTokens")?;
    if !max_output_tokens_val.is_undefined() {
        req_builder.max_output_tokens(max_output_tokens_val.to_number(ctx)?.to_f64() as u32);
    }

    let reasoning_val = opts.get_property_str(ctx, "reasoning")?;
    if !reasoning_val.is_undefined() {
        let reasoning = parse_responses_reasoning(ctx, &reasoning_val)?;
        req_builder.reasoning(reasoning);
    }

    let tools_val = opts.get_property_str(ctx, "tools")?;
    if !tools_val.is_undefined() {
        let tools: Vec<async_openai::types::responses::Tool> =
            serde_json::from_value(tools_val.to_serde_json(ctx).map_err(|_| {
                ctx.throw_type_error("failed to serialize tools");
                -1
            })?)
            .map_err(|e| {
                ctx.throw_type_error(&format!("invalid tools: {e}"));
                -1
            })?;
        if !tools.is_empty() {
            req_builder.tools(tools);
        }
    }

    let tool_choice_val = opts.get_property_str(ctx, "toolChoice")?;
    if !tool_choice_val.is_undefined() {
        let json_val = tool_choice_val.to_serde_json(ctx).map_err(|_| {
            ctx.throw_type_error("failed to serialize tool_choice");
            -1
        })?;
        let tool_choice: async_openai::types::responses::ToolChoiceParam =
            serde_json::from_value(json_val).map_err(|e| {
                ctx.throw_type_error(&format!("invalid tool_choice: {e}"));
                -1
            })?;
        req_builder.tool_choice(tool_choice);
    }

    let metadata_val = opts.get_property_str(ctx, "metadata")?;
    if !metadata_val.is_undefined() {
        let json_val = metadata_val.to_serde_json(ctx).map_err(|_| {
            ctx.throw_type_error("failed to serialize metadata");
            -1
        })?;
        let metadata: std::collections::HashMap<String, String> = serde_json::from_value(json_val)
            .map_err(|e| {
                ctx.throw_type_error(&format!("invalid metadata: {e}"));
                -1
            })?;
        req_builder.metadata(metadata);
    }

    let user_val = opts.get_property_str(ctx, "user")?;
    if let Some(s) = user_val.as_string() {
        req_builder.safety_identifier(s.content().encode_utf8(false).to_string());
    }

    let req = req_builder.build().map_err(|e| {
        ctx.throw_internal_error(&format!("failed to build request: {e}"));
        -1
    })?;

    let base_url = client_arc.base_url.clone();
    let api_key = client_arc.api_key.clone();
    let timeout = client_arc.timeout;

    if is_stream {
        let result_slot = PromiseResultSlot::<ResponsesReply>::new();
        let slot = result_slot.clone();

        tokio::spawn(async move {
            async fn commit_slot(
                slot: &PromiseResultSlot<ResponsesReply>,
                mut msg: ResponsesReply,
            ) {
                loop {
                    match slot.commit(msg) {
                        Ok(()) => return,
                        Err(ret) => {
                            msg = ret;
                            tokio::task::yield_now().await;
                        }
                    }
                }
            }

            let url = format!("{}/responses", base_url);
            let http_client = reqwest::Client::new();
            let request_body = serde_json::to_string(&req).unwrap_or_default();

            let mut builder = http_client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key))
                .body(request_body);
            if let Some(dur) = timeout {
                builder = builder.timeout(dur);
            }
            match builder.send().await {
                Ok(response) => {
                    use futures_util::StreamExt;
                    use tokio::io::AsyncBufReadExt;
                    use tokio_util::io::StreamReader;

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

                            if let Ok(event) = serde_json::from_str::<ResponseStreamEvent>(data) {
                                commit_slot(&slot, ResponsesReply::Event(Box::new(event))).await;
                            }
                        }
                    }
                    commit_slot(&slot, ResponsesReply::Done).await;
                }
                Err(e) => {
                    commit_slot(&slot, ResponsesReply::Error(e.to_string())).await;
                }
            }
        });

        AsyncIterableIterator::create_with_result_slot(
            ctx,
            result_slot,
            |cx, msg| -> Result<Option<JSValue>, i32> {
                match msg {
                    ResponsesReply::Event(event) => stream_event_to_js(cx, &event).map(Some),
                    ResponsesReply::Done => Ok(None),
                    ResponsesReply::Error(e) => {
                        cx.throw_internal_error(&e);
                        Err(-1)
                    }
                }
            },
        )
        .map(JSValue::from)
    } else {
        let result_slot = PromiseResultSlot::<Result<Response, String>>::new();
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
            let url = format!("{}/responses", base_url);
            let http_client = reqwest::Client::new();
            let request_body = serde_json::to_string(&req).unwrap_or_default();

            let mut builder = http_client
                .post(&url)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", api_key))
                .body(request_body);
            if let Some(dur) = timeout {
                builder = builder.timeout(dur);
            }
            let result = match builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    if status.is_success() {
                        match serde_json::from_str::<Response>(&body) {
                            Ok(resp) => Ok(resp),
                            Err(e) => Err(format!("Failed to parse response: {}", e)),
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
