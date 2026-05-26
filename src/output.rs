use async_openai::types::{
    chat::{
        ChatChoice, ChatChoiceStream, ChatCompletionMessageToolCall,
        ChatCompletionMessageToolCallChunk, ChatCompletionMessageToolCalls,
        ChatCompletionResponseMessage, ChatCompletionStreamResponseDelta, CompletionUsage,
        CreateChatCompletionResponse, CreateChatCompletionStreamResponse, FinishReason,
        FunctionCall, FunctionCallStream, FunctionType, Role,
    },
    responses::{
        AssistantRole, FunctionToolCall, InputTokenDetails, OutputContent, OutputItem,
        OutputMessage, OutputMessageContent, OutputStatus, OutputTextContent, OutputTokenDetails,
        ReasoningItem, Response, ResponseCompletedEvent, ResponseContentPartAddedEvent,
        ResponseCreatedEvent, ResponseFunctionCallArgumentsDeltaEvent,
        ResponseOutputItemAddedEvent, ResponseOutputItemDoneEvent,
        ResponseReasoningSummaryTextDeltaEvent, ResponseStreamEvent, ResponseTextDeltaEvent,
        ResponseUsage, Status, SummaryPart, SummaryTextContent,
    },
};
use uuid::Uuid;

use crate::{
    engine::{EngineReply, GeneratedContent, StopReason},
    helpers::now_unix_s,
    llm::{ERR_CANCELLED, ERR_ENGINE_SHUTDOWN},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ParserKind {
    Chat,
    Responses,
}

impl GeneratedContent {
    /// Convert GeneratedContent to chat completion chunk
    pub fn into_chat_chunk(
        self,
        id: String,
        model: String,
        has_tool_call: &mut bool,
    ) -> serde_json::Value {
        match self {
            GeneratedContent::Text(text) => {
                serde_json::to_value(CreateChatCompletionStreamResponse {
                    id,
                    model,
                    created: now_unix_s() as u32,
                    object: "chat.completion.chunk".to_string(),
                    choices: vec![ChatChoiceStream {
                        delta: ChatCompletionStreamResponseDelta {
                            content: Some(text),
                            role: None,
                            tool_calls: None,
                            refusal: None,
                            function_call: None,
                        },
                        index: 0,
                        finish_reason: None,
                        logprobs: None,
                    }],
                    usage: None,
                    system_fingerprint: None,
                    service_tier: None,
                })
                .unwrap()
            }

            GeneratedContent::Reasoning(text) => {
                let mut chunk = serde_json::to_value(CreateChatCompletionStreamResponse {
                    id,
                    model,
                    created: now_unix_s() as u32,
                    object: "chat.completion.chunk".to_string(),
                    choices: vec![ChatChoiceStream {
                        delta: ChatCompletionStreamResponseDelta {
                            content: None,
                            role: None,
                            tool_calls: None,
                            refusal: None,
                            function_call: None,
                        },
                        index: 0,
                        finish_reason: None,
                        logprobs: None,
                    }],
                    usage: None,
                    system_fingerprint: None,
                    service_tier: None,
                })
                .unwrap();

                // Add reasoning_content field
                if let Some(choices) = chunk["choices"].as_array_mut() {
                    if let Some(choice) = choices.first_mut() {
                        if let Some(delta) = choice["delta"].as_object_mut() {
                            delta.insert("reasoning_content".to_string(), text.into());
                        }
                    }
                }

                chunk
            }

            GeneratedContent::ToolCall(repr) => {
                *has_tool_call = true;

                // Parse JSON manually to handle arguments as nested object
                let (name, arguments) = serde_json::from_str::<serde_json::Value>(&repr)
                    .ok()
                    .map(|v| {
                        let name = v
                            .get("name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string());

                        let arguments = v.get("arguments").map(|args| match args {
                            serde_json::Value::String(s) => s.clone(),
                            other => serde_json::to_string(other).unwrap_or_default(),
                        });
                        (name, arguments)
                    })
                    .unwrap_or((None, None));

                serde_json::to_value(CreateChatCompletionStreamResponse {
                    id,
                    model,
                    created: now_unix_s() as u32,
                    object: "chat.completion.chunk".to_string(),
                    choices: vec![ChatChoiceStream {
                        delta: ChatCompletionStreamResponseDelta {
                            content: None,
                            role: None,
                            tool_calls: Some(vec![ChatCompletionMessageToolCallChunk {
                                index: 0,
                                id: Some(format!("call_{}", Uuid::new_v4().simple())),
                                r#type: Some(FunctionType::Function),
                                function: Some(FunctionCallStream { name, arguments }),
                            }]),
                            refusal: None,
                            function_call: None,
                        },
                        index: 0,
                        finish_reason: None,
                        logprobs: None,
                    }],
                    usage: None,
                    system_fingerprint: None,
                    service_tier: None,
                })
                .unwrap()
            }
        }
    }
}

/// Receive EngineReply from rx and direct to OpenAI chat completion chunks.
/// Represents a chunk in OpenAI streaming response
pub(crate) enum ChunkSSE {
    /// Normal content chunk
    Chunk(serde_json::Value),
    /// Error occurred - callback should handle error formatting
    Error(String),
    /// Stream finished marker - callback should handle cleanup (e.g., send "[DONE]")
    Done,
}

pub(crate) fn redirect_engine_reply_to_chat_sse(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    n_prompt_tokens: usize,
    include_usage: bool,
    mut on_chunk: impl FnMut(ChunkSSE) -> bool,
) {
    let mut has_tool_call = false;
    let mut first_chunk = true;

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Content(content)) => {
                let mut chunk = content.into_chat_chunk(
                    respond_id.to_string(),
                    model_name.to_string(),
                    &mut has_tool_call,
                );

                // Set role on first chunk
                if first_chunk {
                    chunk["choices"][0]["delta"]["role"] =
                        serde_json::Value::String("assistant".to_string());
                    first_chunk = false;
                }

                if !on_chunk(ChunkSSE::Chunk(chunk)) {
                    break;
                }
            }
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                // Emit stop message
                let finish_reason = match reason {
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                        if has_tool_call {
                            FinishReason::ToolCalls
                        } else {
                            FinishReason::Stop
                        }
                    }
                    StopReason::MaxTokens => FinishReason::Length,
                };

                let usage = if include_usage {
                    Some(CompletionUsage {
                        prompt_tokens: n_prompt_tokens as u32,
                        completion_tokens: n_completion_tokens as u32,
                        total_tokens: (n_prompt_tokens + n_completion_tokens) as u32,
                        completion_tokens_details: None,
                        prompt_tokens_details: None,
                    })
                } else {
                    None
                };

                let stop_chunk = serde_json::to_value(CreateChatCompletionStreamResponse {
                    id: respond_id.to_string(),
                    model: model_name.to_string(),
                    created: now_unix_s() as u32,
                    object: "chat.completion.chunk".to_string(),
                    choices: vec![ChatChoiceStream {
                        delta: ChatCompletionStreamResponseDelta {
                            content: None,
                            role: None,
                            tool_calls: None,
                            refusal: None,
                            function_call: None,
                        },
                        index: 0,
                        finish_reason: Some(finish_reason),
                        logprobs: None,
                    }],
                    usage,
                    system_fingerprint: None,
                    service_tier: None,
                })
                .unwrap();

                on_chunk(ChunkSSE::Chunk(stop_chunk));

                // Emit done marker
                on_chunk(ChunkSSE::Done);
                break;
            }
            Ok(EngineReply::Cancelled) => {
                on_chunk(ChunkSSE::Error(ERR_CANCELLED.to_string()));
                break;
            }
            Ok(EngineReply::Error(e)) => {
                on_chunk(ChunkSSE::Error(e));
                break;
            }
            Ok(EngineReply::Shutdown) => {
                on_chunk(ChunkSSE::Error(ERR_ENGINE_SHUTDOWN.to_string()));
                break;
            }
            Err(_) => {
                on_chunk(ChunkSSE::Error("stream timed out".to_string()));
                break;
            }
        }
    }
}

/// Parse EngineReply stream into a complete JSON response.
/// EngineReply::Content already carries parsed `GeneratedContent` (Text/Reasoning/ToolCall),
/// so the engine-side OutputParser handles tag/state machine logic — no need for LlmOutputParser.
/// Only non-empty output content is included.
pub(crate) fn collect_engine_reply_to_json(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    kind: ParserKind,
    respond_id: &str,
    model_name: &str,
    n_prompt_tokens: usize,
    include_usage: bool,
) -> Result<serde_json::Value, String> {
    let tm = now_unix_s();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<ChatCompletionMessageToolCalls> = Vec::new();

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Content(content)) => match content {
                GeneratedContent::Text(t) => text.push_str(&t),
                GeneratedContent::Reasoning(t) => reasoning.push_str(&t),
                GeneratedContent::ToolCall(t) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&t) {
                        if let Some(name) = value.get("name").and_then(|v| v.as_str()) {
                            let arguments = match value.get("arguments") {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                                None => String::new(),
                            };
                            calls.push(ChatCompletionMessageToolCalls::Function(
                                ChatCompletionMessageToolCall {
                                    id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                                    function: FunctionCall {
                                        name: name.to_string(),
                                        arguments,
                                    },
                                },
                            ));
                        }
                    }
                }
            },
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                return match kind {
                    ParserKind::Chat => {
                        let finish_reason = match reason {
                            StopReason::MaxTokens => FinishReason::Length,
                            StopReason::PrefillOnly => FinishReason::Stop,
                            StopReason::Eog | StopReason::Word(_) => {
                                if calls.is_empty() {
                                    FinishReason::ToolCalls
                                } else {
                                    FinishReason::Stop
                                }
                            }
                        };

                        let mut resp = serde_json::to_value(CreateChatCompletionResponse {
                            id: respond_id.to_string(),
                            object: "chat.completion".to_string(),
                            created: now_unix_s() as u32,
                            model: model_name.to_string(),
                            choices: vec![ChatChoice {
                                index: 0,
                                message: ChatCompletionResponseMessage {
                                    role: Role::Assistant,
                                    content: if !text.is_empty() { Some(text) } else { None },
                                    tool_calls: if !calls.is_empty() { Some(calls) } else { None },
                                    refusal: None,
                                    function_call: None,
                                    audio: None,
                                    annotations: None,
                                },
                                finish_reason: Some(finish_reason),
                                logprobs: None,
                            }],
                            usage: if include_usage {
                                Some(CompletionUsage {
                                    prompt_tokens: n_prompt_tokens as u32,
                                    completion_tokens: n_completion_tokens as u32,
                                    total_tokens: (n_prompt_tokens + n_completion_tokens) as u32,
                                    completion_tokens_details: None,
                                    prompt_tokens_details: None,
                                })
                            } else {
                                None
                            },
                            system_fingerprint: None,
                            service_tier: None,
                        })
                        .map_err(|e| e.to_string())?;

                        // add extra reasoning_content field
                        if !reasoning.is_empty()
                            && let Some(choices) = resp["choices"].as_array_mut()
                            && let Some(choice) = choices.first_mut()
                            && let Some(msg) = choice["message"].as_object_mut()
                        {
                            msg.insert(
                                "reasoning_content".to_string(),
                                serde_json::Value::String(reasoning),
                            );
                        }

                        Ok(resp)
                    }

                    ParserKind::Responses => {
                        let mut output: Vec<OutputItem> = Vec::new();

                        if !reasoning.is_empty() {
                            output.push(OutputItem::Reasoning(ReasoningItem {
                                id: Some(format!("think_{}", Uuid::new_v4().simple())),
                                summary: vec![SummaryPart::SummaryText(SummaryTextContent {
                                    text: reasoning,
                                })],
                                content: None,
                                encrypted_content: None,
                                status: None,
                            }));
                        }

                        if !text.is_empty() {
                            output.push(OutputItem::Message(OutputMessage {
                                id: format!("text_{}", Uuid::new_v4().simple()),
                                role: AssistantRole::Assistant,
                                status: OutputStatus::Completed,
                                content: vec![OutputMessageContent::OutputText(
                                    OutputTextContent {
                                        text,
                                        annotations: vec![],
                                        logprobs: None,
                                    },
                                )],
                                phase: None,
                            }));
                        }

                        for tc in calls {
                            match tc {
                                ChatCompletionMessageToolCalls::Function(f) => {
                                    output.push(OutputItem::FunctionCall(FunctionToolCall {
                                        id: Some(f.id.clone()),
                                        call_id: f.id,
                                        name: f.function.name,
                                        arguments: f.function.arguments,
                                        namespace: None,
                                        status: Some(OutputStatus::Completed),
                                    }));
                                }
                                ChatCompletionMessageToolCalls::Custom(_) => {}
                            }
                        }

                        let resp = Response {
                            id: respond_id.to_string(),
                            object: "response".to_string(),
                            created_at: tm as _,
                            completed_at: Some(now_unix_s() as _),
                            status: Status::Completed,
                            model: model_name.to_string(),
                            output,
                            usage: if include_usage {
                                Some(ResponseUsage {
                                    input_tokens: n_prompt_tokens as u32,
                                    output_tokens: n_completion_tokens as u32,
                                    total_tokens: (n_prompt_tokens + n_completion_tokens) as u32,
                                    input_tokens_details: InputTokenDetails { cached_tokens: 0 },
                                    output_tokens_details: OutputTokenDetails {
                                        reasoning_tokens: 0,
                                    },
                                })
                            } else {
                                None
                            },
                            background: None,
                            billing: None,
                            conversation: None,
                            error: None,
                            incomplete_details: None,
                            instructions: None,
                            max_output_tokens: None,
                            metadata: None,
                            parallel_tool_calls: None,
                            previous_response_id: None,
                            prompt: None,
                            prompt_cache_key: None,
                            prompt_cache_retention: None,
                            reasoning: None,
                            safety_identifier: None,
                            service_tier: None,
                            temperature: None,
                            text: None,
                            tool_choice: None,
                            tools: None,
                            top_logprobs: None,
                            top_p: None,
                            truncation: None,
                        };

                        serde_json::to_value(resp).map_err(|e| e.to_string())
                    }
                };
            }
            Ok(EngineReply::Cancelled) => return Err(ERR_CANCELLED.to_string()),
            Ok(EngineReply::Error(e)) => return Err(e),
            Ok(EngineReply::Shutdown) => return Err(ERR_ENGINE_SHUTDOWN.to_string()),
            Err(_) => return Err("stream timed out".to_string()),
        }
    }
}

/// Build a minimal Response with only essential fields set.
/// All optional fields are left as None (skipped during serialization).
fn minimal_response(
    id: &str,
    model: &str,
    created_at: u64,
    status: Status,
    usage: Option<ResponseUsage>,
) -> Response {
    Response {
        id: id.to_string(),
        object: "response".to_string(),
        created_at,
        model: model.to_string(),
        status,
        output: vec![],
        usage,
        background: None,
        billing: None,
        conversation: None,
        completed_at: None,
        error: None,
        incomplete_details: None,
        instructions: None,
        max_output_tokens: None,
        metadata: None,
        parallel_tool_calls: None,
        previous_response_id: None,
        prompt: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        reasoning: None,
        safety_identifier: None,
        service_tier: None,
        temperature: None,
        text: None,
        tool_choice: None,
        tools: None,
        top_logprobs: None,
        top_p: None,
        truncation: None,
    }
}

/// Emit a ResponseStreamEvent::ResponseOutputItemAdded.
/// Returns false if the callback indicated the stream should stop.
fn emit_item_added(
    seq: &mut u64,
    output_index: u32,
    item: OutputItem,
    on_chunk: &mut impl FnMut(ChunkSSE) -> bool,
) -> bool {
    let event = ResponseStreamEvent::ResponseOutputItemAdded(ResponseOutputItemAddedEvent {
        sequence_number: *seq,
        output_index,
        item,
    });
    if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&event).unwrap())) {
        return false;
    }
    *seq += 1;
    true
}

/// Direct EngineReply -> Response API event stream (streaming variant).
/// Uses async_openai ResponseStreamEvent standard types for serialization.
pub(crate) fn redirect_engine_reply_to_responses_sse(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    n_prompt_tokens: usize,
    include_usage: bool,
    mut on_chunk: impl FnMut(ChunkSSE) -> bool,
) {
    enum ItemKind {
        Text,
        Reasoning,
    }
    let mut prev_kind: Option<ItemKind> = None;
    let mut msg_id = String::new();
    let mut reasoning_item_id: Option<String> = None;
    let mut seq = 0u64;
    let output_index = 0u32;
    let content_index = 0u32;

    /// Close the current active output item by emitting output_item.done.
    /// Returns false if callback requested stop.
    fn close_current_item(
        seq: &mut u64,
        output_index: u32,
        prev_kind: &mut Option<ItemKind>,
        msg_id: &str,
        reasoning_item_id: &Option<String>,
        on_chunk: &mut impl FnMut(ChunkSSE) -> bool,
    ) -> bool {
        let item = match prev_kind {
            Some(ItemKind::Text) => OutputItem::Message(OutputMessage {
                id: msg_id.to_string(),
                role: AssistantRole::Assistant,
                status: OutputStatus::Completed,
                content: vec![],
                phase: None,
            }),
            Some(ItemKind::Reasoning) => OutputItem::Reasoning(ReasoningItem {
                id: reasoning_item_id.clone(),
                summary: vec![],
                content: None,
                encrypted_content: None,
                status: Some(OutputStatus::Completed),
            }),
            None => return true,
        };
        *prev_kind = None;
        let event = ResponseStreamEvent::ResponseOutputItemDone(ResponseOutputItemDoneEvent {
            sequence_number: *seq,
            output_index,
            item,
        });
        if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&event).unwrap())) {
            return false;
        }
        *seq += 1;
        true
    }

    // Emit response.created
    let created = ResponseStreamEvent::ResponseCreated(ResponseCreatedEvent {
        sequence_number: seq,
        response: minimal_response(
            respond_id,
            model_name,
            now_unix_s() as u64,
            Status::InProgress,
            None,
        ),
    });
    if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&created).unwrap())) {
        return;
    }

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Content(content)) => match content {
                GeneratedContent::Text(t) => {
                    if !matches!(prev_kind, Some(ItemKind::Text)) {
                        if !close_current_item(
                            &mut seq,
                            output_index,
                            &mut prev_kind,
                            &msg_id,
                            &reasoning_item_id,
                            &mut on_chunk,
                        ) {
                            break;
                        }
                        prev_kind = Some(ItemKind::Text);
                        msg_id = format!("msg_{}", uuid::Uuid::new_v4().simple());

                        if !emit_item_added(
                            &mut seq,
                            output_index,
                            OutputItem::Message(OutputMessage {
                                id: msg_id.clone(),
                                role: AssistantRole::Assistant,
                                status: OutputStatus::InProgress,
                                content: vec![],
                                phase: None,
                            }),
                            &mut on_chunk,
                        ) {
                            break;
                        }

                        let part_added = ResponseStreamEvent::ResponseContentPartAdded(
                            ResponseContentPartAddedEvent {
                                sequence_number: seq,
                                item_id: msg_id.clone(),
                                output_index,
                                content_index,
                                part: OutputContent::OutputText(OutputTextContent {
                                    text: String::new(),
                                    annotations: vec![],
                                    logprobs: None,
                                }),
                            },
                        );
                        if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&part_added).unwrap())) {
                            break;
                        }
                        seq += 1;
                    }

                    let delta =
                        ResponseStreamEvent::ResponseOutputTextDelta(ResponseTextDeltaEvent {
                            sequence_number: seq,
                            item_id: msg_id.clone(),
                            output_index,
                            content_index,
                            delta: t,
                            logprobs: None,
                        });
                    if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&delta).unwrap())) {
                        break;
                    }
                    seq += 1;
                }
                GeneratedContent::Reasoning(t) => {
                    if !matches!(prev_kind, Some(ItemKind::Reasoning)) {
                        if !close_current_item(
                            &mut seq,
                            output_index,
                            &mut prev_kind,
                            &msg_id,
                            &reasoning_item_id,
                            &mut on_chunk,
                        ) {
                            break;
                        }
                        prev_kind = Some(ItemKind::Reasoning);
                        let new_id = format!("rsn_{}", uuid::Uuid::new_v4().simple());
                        if !emit_item_added(
                            &mut seq,
                            output_index,
                            OutputItem::Reasoning(ReasoningItem {
                                id: Some(new_id.clone()),
                                summary: vec![],
                                content: None,
                                encrypted_content: None,
                                status: Some(OutputStatus::InProgress),
                            }),
                            &mut on_chunk,
                        ) {
                            break;
                        }
                        reasoning_item_id = Some(new_id);
                    }

                    let rid = reasoning_item_id.as_ref().unwrap().clone();
                    let delta = ResponseStreamEvent::ResponseReasoningSummaryTextDelta(
                        ResponseReasoningSummaryTextDeltaEvent {
                            sequence_number: seq,
                            item_id: rid,
                            output_index,
                            summary_index: 0,
                            delta: t,
                        },
                    );
                    if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&delta).unwrap())) {
                        break;
                    }
                    seq += 1;
                }
                GeneratedContent::ToolCall(t) => {
                    if !close_current_item(
                        &mut seq,
                        output_index,
                        &mut prev_kind,
                        &msg_id,
                        &reasoning_item_id,
                        &mut on_chunk,
                    ) {
                        break;
                    }
                    match serde_json::from_str::<serde_json::Value>(&t) {
                        Ok(value) => {
                            if let Some(obj) = value.as_object().filter(|o| o.len() == 2) {
                                if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                                    let call_id = format!("call_{}", uuid::Uuid::new_v4().simple());
                                    let fc_id = format!("fc_{}", uuid::Uuid::new_v4().simple());
                                    let arguments = match obj.get("arguments") {
                                        Some(serde_json::Value::String(s)) => s.clone(),
                                        Some(other) => {
                                            serde_json::to_string(other).unwrap_or_default()
                                        }
                                        None => String::new(),
                                    };

                                    let tool_call = FunctionToolCall {
                                        id: Some(fc_id.clone()),
                                        call_id: call_id.clone(),
                                        namespace: None,
                                        name: name.to_string(),
                                        arguments: arguments.clone(),
                                        status: None,
                                    };

                                    if !emit_item_added(
                                        &mut seq,
                                        output_index,
                                        OutputItem::FunctionCall(tool_call),
                                        &mut on_chunk,
                                    ) {
                                        break;
                                    }

                                    let args_delta =
                                        ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
                                            ResponseFunctionCallArgumentsDeltaEvent {
                                                sequence_number: seq,
                                                item_id: fc_id,
                                                output_index,
                                                delta: arguments,
                                            },
                                        );
                                    if !on_chunk(ChunkSSE::Chunk(
                                        serde_json::to_value(&args_delta).unwrap(),
                                    )) {
                                        break;
                                    }
                                    seq += 1;
                                }
                            }
                        }
                        Err(e) => {
                            if !on_chunk(ChunkSSE::Error(format!("tool_call parse error: {e}"))) {
                                break;
                            }
                        }
                    }
                }
            },
            Ok(EngineReply::Stop(_reason, n_completion_tokens)) => {
                if !close_current_item(
                    &mut seq,
                    output_index,
                    &mut prev_kind,
                    &msg_id,
                    &reasoning_item_id,
                    &mut on_chunk,
                ) {
                    break;
                }
                let usage = if include_usage {
                    Some(ResponseUsage {
                        input_tokens: n_prompt_tokens as u32,
                        output_tokens: n_completion_tokens as u32,
                        total_tokens: (n_prompt_tokens + n_completion_tokens) as u32,
                        input_tokens_details: InputTokenDetails { cached_tokens: 0 },
                        output_tokens_details: OutputTokenDetails {
                            reasoning_tokens: 0,
                        },
                    })
                } else {
                    None
                };
                let completed = ResponseStreamEvent::ResponseCompleted(ResponseCompletedEvent {
                    sequence_number: seq,
                    response: minimal_response(
                        respond_id,
                        model_name,
                        now_unix_s() as u64,
                        Status::Completed,
                        usage,
                    ),
                });
                if !on_chunk(ChunkSSE::Chunk(serde_json::to_value(&completed).unwrap())) {
                    break;
                }
                on_chunk(ChunkSSE::Done);
                break;
            }
            Ok(EngineReply::Cancelled) => {
                on_chunk(ChunkSSE::Error(ERR_CANCELLED.to_string()));
                break;
            }
            Ok(EngineReply::Error(e)) => {
                on_chunk(ChunkSSE::Error(e));
                break;
            }
            Ok(EngineReply::Shutdown) => {
                on_chunk(ChunkSSE::Error(ERR_ENGINE_SHUTDOWN.to_string()));
                break;
            }
            Err(_) => {
                on_chunk(ChunkSSE::Error("stream timed out".to_string()));
                break;
            }
        }
    }
}
