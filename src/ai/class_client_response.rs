// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use std::{sync::Arc, time::Duration};

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

use crate::whatwg::{
    abort::{signal_is_aborted, signal_notify, signal_reason},
    dom::make_dom_exception,
};

use super::{
    MAGIC_RESPONSES_STREAM,
    class_client::{ABORT_MSG, LlmStreamFetcher, StreamCancel, get_client, parse_call_opts},
};

enum ResponsesReply {
    Event(Box<ResponseStreamEvent>),
    Done,
    Error(String),
}

pub(super) fn parse_responses_input(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<InputParam, i32> {
    if let Some(s) = val.as_string(true) {
        return Ok(InputParam::Text(s.content().encode_utf8(false).to_string()));
    };

    if let Some(arr) = val.as_object_Array() {
        let mut items = Vec::with_capacity(arr.payload().len());

        for item in arr.payload().values() {
            let obj = item
                .as_object()
                .ok_or_else(|| ctx.throw_type_error("input[] item must be object"))?;

            let role_val = obj.get_property_str(ctx, "role")?;
            let role_str = role_val
                .as_string(true)
                .map(|s| s.content().encode_utf8(false).to_string())
                .ok_or_else(|| ctx.throw_type_error("input[].role must be string"))?;
            let role = match role_str.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                "developer" => Role::Developer,
                _ => {
                    return Err(ctx.throw_type_error(&format!("unsupported role: {role_str}")));
                }
            };

            let content_val = obj.get_property_str(ctx, "content")?;
            let content = if content_val.is_undefined_or_null() {
                String::new()
            } else if let Some(s) = content_val.as_string(true) {
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

    Err(ctx.throw_type_error("input must be a string or an array"))
}

pub(super) fn parse_responses_reasoning(
    ctx: &mut ScopedContext,
    val: &JSValue,
) -> Result<Reasoning, i32> {
    let json_val = val
        .to_serde_json(ctx)
        .map_err(|_| ctx.throw_type_error("failed to serialize reasoning"))?;
    serde_json::from_value::<Reasoning>(json_val)
        .map_err(|e| ctx.throw_type_error(&format!("invalid reasoning: {e}")))
}

fn response_to_js(ctx: &mut ScopedContext, resp: &Response) -> JSResult {
    let val = serde_json::to_value(resp)
        .map_err(|e| ctx.throw_internal_error(&format!("serialize response failed: {e}")))?;
    JSValue::from_serde_json(ctx, &val)
}

fn stream_event_to_js(ctx: &mut ScopedContext, event: &ResponseStreamEvent) -> JSResult {
    let val = serde_json::to_value(event)
        .map_err(|e| ctx.throw_internal_error(&format!("serialize stream event failed: {e}")))?;
    JSValue::from_serde_json(ctx, &val)
}

pub(super) fn js_client_responses(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_RESPONSES_STREAM;

    // arg(0): the pure-data request; arg(1): control-only opts.
    let req_val = cc.arg(0);
    let req_obj = req_val
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("responses() requires a request object"))?;
    let opts = parse_call_opts(ctx, &cc)?;
    let abort_notify = signal_notify(&opts.signal);
    let already_aborted = signal_is_aborted(&opts.signal);

    let input_val = req_obj.get_property_str(ctx, "input")?;
    let input = parse_responses_input(ctx, &input_val)?;

    let model_val = req_obj.get_property_str(ctx, "model")?;
    let model = if let Some(s) = model_val.as_string(true) {
        s.content().encode_utf8(false).to_string()
    } else {
        client.common.model_name.clone()
    };

    let mut req_builder = CreateResponseArgs::default();
    req_builder.input(input).model(&model);
    // Streaming branch must ask the server for SSE 鈥?without `stream: true`
    // the server returns a plain JSON body and the iterator sees no events.
    if is_stream {
        req_builder.stream(true);
    }

    let instructions_val = req_obj.get_property_str(ctx, "instructions")?;
    if let Some(s) = instructions_val.as_string(true) {
        req_builder.instructions(s.content().encode_utf8(false).to_string());
    }

    let temperature_val = req_obj.get_property_str(ctx, "temperature")?;
    if !temperature_val.is_undefined() {
        req_builder.temperature(temperature_val.to_number(ctx)?.to_f64() as f32);
    }

    let top_p_val = req_obj.get_property_str(ctx, "topP")?;
    if !top_p_val.is_undefined() {
        req_builder.top_p(top_p_val.to_number(ctx)?.to_f64() as f32);
    }

    let max_output_tokens_val = req_obj.get_property_str(ctx, "maxOutputTokens")?;
    if !max_output_tokens_val.is_undefined() {
        req_builder.max_output_tokens(max_output_tokens_val.to_number(ctx)?.to_f64() as u32);
    }

    let reasoning_val = req_obj.get_property_str(ctx, "reasoning")?;
    if !reasoning_val.is_undefined() {
        let reasoning = parse_responses_reasoning(ctx, &reasoning_val)?;
        req_builder.reasoning(reasoning);
    }

    let tools_val = req_obj.get_property_str(ctx, "tools")?;
    if !tools_val.is_undefined() {
        let tools: Vec<async_openai::types::responses::Tool> = serde_json::from_value(
            tools_val
                .to_serde_json(ctx)
                .map_err(|_| ctx.throw_type_error("failed to serialize tools"))?,
        )
        .map_err(|e| ctx.throw_type_error(&format!("invalid tools: {e}")))?;
        if !tools.is_empty() {
            req_builder.tools(tools);
        }
    }

    let tool_choice_val = req_obj.get_property_str(ctx, "toolChoice")?;
    if !tool_choice_val.is_undefined() {
        let json_val = tool_choice_val
            .to_serde_json(ctx)
            .map_err(|_| ctx.throw_type_error("failed to serialize tool_choice"))?;
        let tool_choice: async_openai::types::responses::ToolChoiceParam =
            serde_json::from_value(json_val)
                .map_err(|e| ctx.throw_type_error(&format!("invalid tool_choice: {e}")))?;
        req_builder.tool_choice(tool_choice);
    }

    let metadata_val = req_obj.get_property_str(ctx, "metadata")?;
    if !metadata_val.is_undefined() {
        let json_val = metadata_val
            .to_serde_json(ctx)
            .map_err(|_| ctx.throw_type_error("failed to serialize metadata"))?;
        let metadata: std::collections::HashMap<String, String> = serde_json::from_value(json_val)
            .map_err(|e| ctx.throw_type_error(&format!("invalid metadata: {e}")))?;
        req_builder.metadata(metadata);
    }

    let user_val = req_obj.get_property_str(ctx, "user")?;
    if let Some(s) = user_val.as_string(true) {
        req_builder.safety_identifier(s.content().encode_utf8(false).to_string());
    }

    let req = req_builder
        .build()
        .map_err(|e| ctx.throw_internal_error(&format!("failed to build request: {e}")))?;

    let base_url = client.common.base_url.clone();
    let api_key = client.common.api_key.clone();
    // Per-request timeout (seconds) overrides the client-level default.
    let timeout = opts
        .timeout
        .map(Duration::from_secs)
        .unwrap_or(client.common.timeout);

    let url = format!("{}/responses", base_url);
    let http_client = client.common.http.clone();
    let request_body = serde_json::to_string(&req).unwrap_or_default();
    let mut req_builder = http_client
        .post(&url)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(request_body);

    if !api_key.is_empty() {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
    }

    if is_stream {
        let stall_limit = opts.stallTimeout.map(Duration::from_secs);
        let result_slot = PromiseResultSlot::<ResponsesReply>::new();
        // Commits from the reading task wake the parked event loop so the
        // custom fetcher below polls the slot promptly.
        result_slot.attach_runtime_wake(ctx);
        let cancel = Arc::new(StreamCancel::new());
        let slot = result_slot.clone();
        let task_cancel = cancel.clone();

        tokio::spawn(async move {
            // `return()`/`break` on the JS iterator fires `task_cancel`, dropping
            // `body` 鈥?and with it the in-flight response 鈥?closing the upstream.
            let body = async {
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

                // Fail-fast: a pre-aborted signal never leaves the process.
                if already_aborted {
                    commit_slot(&slot, ResponsesReply::Error(ABORT_MSG.to_string())).await;
                    return;
                }

                match req_builder.send().await {
                    Ok(response) => {
                        use futures_util::StreamExt;
                        use tokio::io::AsyncBufReadExt;
                        use tokio_util::io::StreamReader;

                        let byte_stream = response
                            .bytes_stream()
                            .map(|r| r.map_err(std::io::Error::other));
                        let reader = StreamReader::new(byte_stream);
                        let mut lines = tokio::io::BufReader::new(reader).lines();

                        // Stall guard: each line read races a per-call timeout so
                        // any received byte resets it. On fire, an Error is
                        // committed and Done is skipped (stalled output dropped).
                        let mut stall_aborted = false;

                        loop {
                            // Fast path: no abort signal and no stall guard 鈥?                            // read the next line directly.
                            let next_result: std::io::Result<Option<String>> = if abort_notify
                                .is_none()
                                && stall_limit.is_none()
                            {
                                lines.next_line().await
                            } else {
                                let line_fut = async {
                                    match stall_limit {
                                        Some(stall) => {
                                            tokio::time::timeout(stall, lines.next_line())
                                                .await
                                                .map_err(|_| ())
                                        }
                                        None => Ok(lines.next_line().await),
                                    }
                                };
                                let next = match &abort_notify {
                                    Some(n) => tokio::select! {
                                        biased;
                                        _ = n.notified() => {
                                            commit_slot(&slot, ResponsesReply::Error(ABORT_MSG.to_string())).await;
                                            break;
                                        }
                                        r = line_fut => r,
                                    },
                                    None => line_fut.await,
                                };
                                match next {
                                    Ok(inner) => inner,
                                    Err(()) => {
                                        stall_aborted = true;
                                        commit_slot(
                                            &slot,
                                            ResponsesReply::Error(format!(
                                                "llm stream stall timeout: no data for {}s",
                                                stall_limit.map(|d| d.as_secs()).unwrap_or(0)
                                            )),
                                        )
                                        .await;
                                        break;
                                    }
                                }
                            };

                            // EOF/read error ends the stream silently (legacy).
                            let Ok(Some(line)) = next_result else {
                                break;
                            };

                            let line = line.trim();
                            if let Some(data) = line.strip_prefix("data: ") {
                                if data == "[DONE]" {
                                    break;
                                }

                                if let Ok(event) = serde_json::from_str::<ResponseStreamEvent>(data)
                                {
                                    commit_slot(&slot, ResponsesReply::Event(Box::new(event)))
                                        .await;
                                }
                            }
                        }
                        if !stall_aborted {
                            commit_slot(&slot, ResponsesReply::Done).await;
                        }
                    }
                    Err(e) => {
                        commit_slot(&slot, ResponsesReply::Error(e.to_string())).await;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = task_cancel.notified() => {}
                _ = body => {}
            }
        });

        AsyncIterableIterator::create(
            ctx,
            LlmStreamFetcher::new(
                result_slot,
                cancel,
                JSValue::UNDEFINED,
                |cx, msg| -> Result<Option<JSValue>, i32> {
                    match msg {
                        ResponsesReply::Event(event) => stream_event_to_js(cx, &event).map(Some),
                        ResponsesReply::Done => Ok(None),
                        ResponsesReply::Error(e) => Err(cx.throw_internal_error(&e)),
                    }
                },
            ),
        )
        .map(JSValue::from)
    } else {
        // Fail-fast: reject with the signal's reason without sending.
        if already_aborted {
            let reason = match signal_reason(&opts.signal) {
                Some(r) => r,
                None => make_dom_exception(ctx, "AbortError", "The operation was aborted.")?,
            };
            return Promise::from_reject_value(ctx, reason).map(JSValue::from);
        }

        let result_slot = PromiseResultSlot::<Result<Response, String>>::new();
        let promise =
            Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, result| -> JSResult {
                match result {
                    Ok(resp) => response_to_js(cx, &resp),
                    Err(e) => Err(cx.throw_internal_error(&e)),
                }
            })?;

        tokio::spawn(async move {
            // Abort races the whole request; dropping `result_fut` closes the
            // upstream connection, so generation stops server-side too.
            let result_fut = async {
                match req_builder.send().await {
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
                }
            };

            let result = match abort_notify {
                Some(n) => tokio::select! {
                    biased;
                    _ = n.notified() => Err(ABORT_MSG.to_string()),
                    r = result_fut => r,
                },
                None => result_fut.await,
            };
            let _ = result_slot.commit(result);
        });

        Ok(promise.into())
    }
}
