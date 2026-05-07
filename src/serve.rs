use std::sync::{Arc, Mutex};

use async_openai::types::{
    ChatChoice, ChatChoiceStream, ChatCompletionMessageToolCall,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionResponseMessage,
    ChatCompletionStreamResponseDelta, ChatCompletionTool, ChatCompletionToolChoiceOption,
    ChatCompletionToolType, CompletionUsage, CreateChatCompletionRequest,
    CreateChatCompletionResponse, CreateChatCompletionStreamResponse, FinishReason, FunctionCall,
    Role,
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::sse::{Event, Sse},
    routing::post,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    chat_response::{ChatResponseParser, ChatStreamItem, chat_response_listener},
    engine::{Engine, EngineReply, GrammarConfig, OutputMode, StopReason, TaskConfig},
    helpers::{build_chat_prompt_tokens, compile_tools_grammar, now_unix_s, to_lark_tool_choice},
};

#[derive(Clone)]
pub(crate) struct AppState {
    pub engine: Arc<Mutex<Engine>>,
}

/// Start http server with Engine (in current thread), blocks until shutdown signal.
pub(crate) async fn http_serve(
    engine: Arc<Mutex<Engine>>,
    host: &str,
    port: u16,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
    let state = AppState { engine };

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_handler))
        .route("/health", post(health_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(|e| format!("failed to bind {host}:{port}: {e}"))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async { drop(shutdown_rx.await) })
        .await
        .map_err(|e| format!("server error: {e}"))
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<CreateChatCompletionRequest>,
) -> Result<
    Sse<impl futures_util::stream::Stream<Item = Result<Event, String>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let is_stream = req.stream.unwrap_or(false);

    let messages = &req.messages;
    let tools = req.tools.clone();
    let tool_choice = req.tool_choice.clone();
    let parallel_tool_calls = req.parallel_tool_calls;
    let reasoning_effort = req.reasoning_effort;
    let response_format = req.response_format;
    let max_tokens = req.max_tokens.unwrap_or(0) as usize;
    let temperature = req.temperature.unwrap_or(0.75) as f32;
    let top_p = req.top_p.unwrap_or(1.0) as f32;
    let stop = req.stop.clone();
    let include_usage = req
        .stream_options
        .as_ref()
        .map(|o| o.include_usage)
        .unwrap_or(false);

    let prompt_tokens = build_chat_prompt_tokens(
        state.engine.lock().unwrap().model(),
        messages,
        tools.as_deref(),
        tool_choice.as_ref(),
        parallel_tool_calls,
        reasoning_effort.as_ref(),
        response_format.as_ref(),
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": {"message": e.to_string()}})),
        )
    })?;

    let mut task_config = TaskConfig::new()
        .max_tokens(max_tokens)
        .temperature(temperature)
        .top_p(top_p)
        .output_mode(if is_stream {
            OutputMode::Continuous
        } else {
            OutputMode::OneShot
        });

    if let Some(stop_words) = stop.as_ref() {
        let words: Vec<String> = match stop_words {
            async_openai::types::Stop::String(s) => vec![s.clone()],
            async_openai::types::Stop::StringArray(arr) => arr.clone(),
        };
        if !words.is_empty() {
            task_config = task_config.stop_words(words);
        }
    }

    if let Some(tools) = tools.as_ref().filter(|t| !t.is_empty()) {
        // build grammar for tools
        let tc = tool_choice
            .as_ref()
            .map(to_lark_tool_choice)
            .unwrap_or(llama_cpp_ffi::ToolChoice::Auto);

        if tc != llama_cpp_ffi::ToolChoice::None {
            task_config = task_config.grammar(GrammarConfig {
                lark_grammar: compile_tools_grammar(tools, tc),
                tool_choice: tc,
            });
        }
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let model_name = state.engine.lock().unwrap().model_name().to_string();

    let rx = state
        .engine
        .lock()
        .unwrap()
        .push_task_with_params(prompt_tokens, task_config, None, false)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": e.to_string()}})),
            )
        })?;

    // Use crossbeam::channel (not tokio::sync::mpsc) because the producer runs
    // inside chat_response_listener's synchronous callback, which blocks the
    // current thread via crossbeam::Receiver::recv_timeout. If we used
    // tokio::sync::mpsc::blocking_send inside a tokio::spawn task, it would
    // panic with "Cannot block the current thread from within a runtime".
    // Using std::thread::spawn + crossbeam::channel avoids this entirely.
    let (event_tx, event_rx) = crossbeam::channel::bounded::<Result<Event, String>>(8);

    if is_stream {
        // Bridge crossbeam Receiver -> tokio stream for SSE
        std::thread::spawn(move || {
            let mut parser = ChatResponseParser::new();
            if reasoning_effort.is_some() {
                parser.push_str("<think>");
            }

            chat_response_listener(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                include_usage,
                n_prompt_tokens,
                parser,
                |item| {
                    let event = match item {
                        ChatStreamItem::Chunk(chunk) => {
                            let data = serde_json::to_string(&chunk).unwrap_or_default();
                            Event::default().data(data)
                        }
                        ChatStreamItem::Done => Event::default().data("[DONE]"),
                        ChatStreamItem::Error(e) => {
                            let data = serde_json::to_string(&json!({"error": {"message": e}}))
                                .unwrap_or_default();
                            Event::default().data(data)
                        }
                    };
                    event_tx.send(Ok(event)).is_ok()
                },
            );
        });
    } else {
        // Unary: collect all EngineReply, build final response
        std::thread::spawn(move || {
            let result = collect_chat_response(
                rx,
                std::time::Duration::from_secs(u64::MAX),
                &respond_id,
                &model_name,
                include_usage,
                n_prompt_tokens,
            );
            match result {
                Ok(data) => {
                    let _ = event_tx.send(Ok(Event::default().data(data)));
                }
                Err(e) => {
                    let data = serde_json::to_string(&json!({"error": {"message": e}}))
                        .unwrap_or_default();
                    let _ = event_tx.send(Ok(Event::default().data(data)));
                }
            }
            let _ = event_tx.send(Ok(Event::default().data("[DONE]")));
        });
    }

    let stream = crossbeam_to_async_stream(event_rx);
    Ok(Sse::new(stream))
}

/// Bridge a crossbeam::channel::Receiver into a tokio async Stream for SSE.
///
/// axum's Sse requires a `Stream<Item = Result<Event, String>>` that implements
/// `Send`. crossbeam::channel::Receiver does not implement tokio's Stream trait,
/// so we bridge it via a tokio::sync::mpsc::unbounded_channel: a dedicated
/// std::thread reads from the crossbeam channel (blocking recv) and forwards
/// each item into the tokio channel, which is then wrapped as a Stream via
/// UnboundedReceiverStream.
fn crossbeam_to_async_stream(
    rx: crossbeam::channel::Receiver<Result<Event, String>>,
) -> impl futures_util::stream::Stream<Item = Result<Event, String>> {
    let (tx, rx_async) = tokio::sync::mpsc::unbounded_channel();
    
    std::thread::spawn(move || {
        while let Ok(item) = rx.recv() {
            if tx.send(item).is_err() {
                break;
            }
        }
    });

    tokio_stream::wrappers::UnboundedReceiverStream::new(rx_async)
}

fn collect_chat_response(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    include_usage: bool,
    n_prompt_tokens: usize,
) -> Result<String, String> {
    let mut full_text = String::new();
    let mut n_completion_tokens = 0usize;

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Text(t)) => {
                full_text.push_str(&t);
                n_completion_tokens += 1;
            }
            Ok(EngineReply::Stop(reason, _)) => {
                let finish_reason = match reason {
                    StopReason::MaxTokens => FinishReason::Length,
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                        FinishReason::Stop
                    }
                };

                let tool_calls = parse_tool_calls_from_text(&full_text);

                let resp = CreateChatCompletionResponse {
                    id: respond_id.to_string(),
                    object: "chat.completion".to_string(),
                    created: now_unix_s() as u32,
                    model: model_name.to_string(),
                    choices: vec![ChatChoice {
                        index: 0,
                        message: ChatCompletionResponseMessage {
                            content: Some(full_text),
                            tool_calls,
                            role: Role::Assistant,
                            refusal: None,
                            function_call: None,
                            audio: None,
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
                };

                return serde_json::to_string(&resp).map_err(|e| e.to_string());
            }
            Ok(EngineReply::Cancelled) => return Err("cancelled".to_string()),
            Ok(EngineReply::Error(e)) => return Err(e),
            Ok(EngineReply::Shutdown) => return Err("engine shutdown".to_string()),
            Err(_) => return Err("stream timed out".to_string()),
        }
    }
}

fn parse_tool_calls_from_text(text: &str) -> Option<Vec<ChatCompletionMessageToolCall>> {
    let mut calls = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find("<tool_call>") {
        let content_start = start + "<tool_call>".len();
        let end = remaining[content_start..].find("</tool_call>")?;
        let json_str = &remaining[content_start..content_start + end];

        if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
            let name = value["name"].as_str().map(|s| s.to_string());
            let args_value = value["arguments"].clone();

            if let Some(name) = name {
                let Ok(arguments) = serde_json::to_string(&args_value) else {
                    eprint!("[warn] bad arguments in tool_call: {}", args_value);
                    return None;
                };

                calls.push(ChatCompletionMessageToolCall {
                    id: format!("call_{}", Uuid::new_v4().simple()),
                    r#type: ChatCompletionToolType::Function,
                    function: FunctionCall { name, arguments },
                });
            }
        }

        remaining = &remaining[content_start + end + "</tool_call>".len()..];
    }

    if !calls.is_empty() { Some(calls) } else { None }
}
