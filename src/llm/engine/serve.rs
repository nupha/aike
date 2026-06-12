// Engine OpenAI-compatible HTTP handlers.
//
// Provides handler functions and `engine_routes()` which returns RouteEntry
// items for all Engine-related routes (/v1/chat/completions, /v1/models, etc).

use std::{
    ffi::CString,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_openai::types::chat::{CreateChatCompletionRequest, ReasoningEffort};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use crate::{
    helpers::{
        PromptThinkPattern, apply_chat_template, compute_keep_tokens, detect_prompt_think_pattern,
        now_unix_s, to_lark_tool_choice,
    },
    http::server::RouteEntry,
    llm::engine::{Engine, EngineReply, EngineTask, GrammarConfig, TaskConfig},
    output::{
        ChunkSSE, ParserKind, collect_engine_reply_to_json, redirect_engine_reply_to_chat_sse,
    },
};

#[cfg(feature = "enable_eval_log")]
use crate::eval_logger::{EvalLogEntry, ToolCallLog, get_eval_logger};

pub(crate) const ERR_CANCELLED: &str = "cancelled";
pub(crate) const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";
pub(crate) const DEFAULT_TEMPERATURE: f32 = 0.75;
pub(crate) const DEFAULT_TOP_P: f32 = 1.0;
pub(crate) const STREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// A guard that cancels an engine task on drop unless disarmed.
/// Used to ensure tasks are cancelled when the HTTP client disconnects.
pub(crate) struct CancelGuard {
    engine: Option<Arc<Mutex<Engine>>>,
    task_id: usize,
}

impl CancelGuard {
    pub(crate) fn new(engine: Arc<Mutex<Engine>>, task_id: usize) -> Self {
        Self {
            engine: Some(engine),
            task_id,
        }
    }

    /// Disarm the guard — the task will NOT be cancelled on drop.
    pub(crate) fn disarm(&mut self) {
        self.engine.take();
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.take() {
            log::info!(
                "[cancel] client disconnected, cancelling task id={}",
                self.task_id
            );
            let _ = engine.lock().unwrap().cancel_task(self.task_id);
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: OpenAiError,
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiError {
    pub(crate) message: String,
    #[serde(rename = "type")]
    pub(crate) type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<String>,
}

pub(crate) fn oai_error(message: impl Into<String>, type_: impl Into<String>) -> ErrorResponse {
    ErrorResponse {
        error: OpenAiError {
            message: message.into(),
            type_: type_.into(),
            param: None,
            code: None,
        },
    }
}

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

/// Build all Engine-related RouteEntry items.
/// Handlers are resolved with state (.with_state) so RouteEntry contains MethodRouter<()>.
pub(crate) fn openai_engine_routes(engine: Arc<Mutex<Engine>>) -> Vec<RouteEntry> {
    vec![
        RouteEntry {
            path: "/".into(),
            handler: get(handle_root),
        },
        RouteEntry {
            path: "/health".into(),
            handler: get(handle_health).with_state(engine.clone()),
        },
        RouteEntry {
            path: "/models".into(),
            handler: get(handle_models).with_state(engine.clone()),
        },
        RouteEntry {
            path: "/v1/health".into(),
            handler: get(handle_health).with_state(engine.clone()),
        },
        RouteEntry {
            path: "/v1/models".into(),
            handler: get(handle_models).with_state(engine.clone()),
        },
        RouteEntry {
            path: "/v1/chat/completions".into(),
            handler: post(handle_chat_completions).with_state(engine.clone()),
        },
        RouteEntry {
            path: "/v1/responses".into(),
            handler: post(crate::responses::handle_responses).with_state(engine.clone()),
        },
    ]
}

async fn handle_root() -> impl IntoResponse {
    Json(json!({"name":"llm server"}))
}

async fn handle_health(State(engine): State<Arc<Mutex<Engine>>>) -> impl IntoResponse {
    let alive = engine.lock().unwrap().is_alive();
    if alive {
        Json(json!({"status": "ok"})).into_response()
    } else {
        let status = StatusCode::SERVICE_UNAVAILABLE;
        (
            status,
            Json(json!({"status": "error", "message": "engine worker has crashed"})),
        )
            .into_response()
    }
}

async fn handle_models(State(engine): State<Arc<Mutex<Engine>>>) -> impl IntoResponse {
    let model_name = engine.lock().unwrap().model_name().to_string();
    let data = vec![ModelInfo {
        id: model_name,
        object: "model",
        created: now_unix_s(),
        owned_by: "local",
    }];

    Json(json!({
        "object": "list",
        "data": data,
    }))
}

async fn handle_chat_completions(
    State(engine): State<Arc<Mutex<Engine>>>,
    Json(req): Json<CreateChatCompletionRequest>,
) -> Response {
    // Quick check: if the engine worker has crashed, reject immediately.
    if !engine.lock().unwrap().is_alive() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(oai_error("engine worker has crashed", "server_error")),
        )
            .into_response();
    }

    #[cfg(feature = "enable_eval_log")]
    let start_time = std::time::Instant::now();

    #[cfg(feature = "enable_eval_log")]
    let request_id = format!("req_{}", Uuid::new_v4().simple());

    if req.messages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error(
                "\"messages\" must not be empty",
                "invalid_request_error",
            )),
        )
            .into_response();
    }

    let is_stream = req.stream.unwrap_or(false);
    let enable_thinking = req.reasoning_effort.clone().unwrap_or_default() != ReasoningEffort::None;

    // Extract model info from engine under lock, then release lock
    let (model, parse_reasoning, default_max_tokens, model_name, input_tx) = {
        let e = engine.lock().unwrap();
        (
            e.model().clone(),
            e.parse_reasoning(),
            e.default_max_tokens(),
            e.model_name().to_string(),
            e.input_tx.clone(),
        )
    };

    let output_prepend: Vec<i32>;

    // Build prompt tokens
    let prompt_tokens = {
        match apply_chat_template(
            &model,
            &req.messages,
            req.tools.as_deref(),
            req.tool_choice.as_ref(),
            req.parallel_tool_calls,
            req.reasoning_effort.clone().unwrap_or_default(),
            req.response_format.as_ref(),
        ) {
            Ok(prompt) => {
                output_prepend = if enable_thinking && parse_reasoning {
                    let tags = model.arch().think_boundaries();
                    if detect_prompt_think_pattern(&prompt, tags.0, tags.1)
                        == PromptThinkPattern::Open
                    {
                        model.tokenize(CString::new(tags.0).unwrap().as_c_str(), false, false)
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                model.tokenize(CString::new(prompt).unwrap().as_c_str(), false, true)
            }
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(oai_error(e, "invalid_request_error")),
                )
                    .into_response();
            }
        }
    };

    if prompt_tokens.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error(
                "prompt tokenized to zero tokens",
                "invalid_request_error",
            )),
        )
            .into_response();
    }

    let keep_tokens = compute_keep_tokens(&model, &req.messages);

    let mut task_config = TaskConfig::new()
        .max_tokens(
            req.max_completion_tokens
                .unwrap_or(default_max_tokens as u32) as usize,
        )
        .temperature(req.temperature.unwrap_or(DEFAULT_TEMPERATURE))
        .top_p(req.top_p.unwrap_or(DEFAULT_TOP_P))
        .enable_output_parser(parse_reasoning)
        .output_stream(is_stream)
        .keep_tokens(keep_tokens);

    if let Some(stop_words) = req.stop.as_ref() {
        let words: Vec<String> = match stop_words {
            async_openai::types::chat::StopConfiguration::String(s) => vec![s.clone()],
            async_openai::types::chat::StopConfiguration::StringArray(arr) => arr.clone(),
        };
        if !words.is_empty() {
            task_config = task_config.stop_words(words);
        }
    }

    if let Some(tools) = req.tools.as_ref().filter(|t| !t.is_empty()) {
        let tc = req
            .tool_choice
            .as_ref()
            .map(to_lark_tool_choice)
            .unwrap_or(llama_cpp_ffi::ToolChoice::Auto);

        if tc != llama_cpp_ffi::ToolChoice::None {
            task_config = task_config.grammar(GrammarConfig::new(&model, tools, tc));
        }
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("chatcmpl-{}", Uuid::new_v4().simple());

    // Save task_id before moving task_config into EngineTask.
    let task_id = task_config.task_id;

    // Push task via channel to avoid acquiring engine.lock()
    // (worker thread may hold the lock for extended periods during inference).
    let (output_tx, output_rx) = crossbeam::channel::bounded::<EngineReply>(1);
    let task = Box::new(EngineTask {
        config: task_config,
        prompt_tokens,
        output_prepend,
        output_tx,
        timestamp: None,
    });

    let rx = match input_tx.send(task) {
        Ok(()) => output_rx,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(oai_error(e.to_string(), "server_error")),
            )
                .into_response();
        }
    };

    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|o| o.include_usage)
        .unwrap_or(false);

    if is_stream {
        // streaming - sse
        log::debug!("[http] streaming path: spawning blocking task for SSE");
        let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();

        #[cfg(feature = "enable_eval_log")]
        let req_clone = req.clone();
        #[cfg(feature = "enable_eval_log")]
        let model_name_clone = model_name.clone();
        #[cfg(feature = "enable_eval_log")]
        let n_prompt_tokens_clone = n_prompt_tokens;
        #[cfg(feature = "enable_eval_log")]
        let request_id_clone = request_id.clone();
        #[cfg(feature = "enable_eval_log")]
        let start_time_clone = start_time;

        let engine_for_worker = engine.clone();
        tokio::task::spawn_blocking(move || {
            let mut client_disconnected = false;

            redirect_engine_reply_to_chat_sse(
                rx,
                STREAM_TIMEOUT,
                &respond_id,
                &model_name,
                n_prompt_tokens,
                req.stream_options
                    .as_ref()
                    .and_then(|o| o.include_usage)
                    .unwrap_or(false),
                |chunk| {
                    match chunk {
                        ChunkSSE::Chunk(data) => {
                            let serialized = serde_json::to_string(&data).unwrap();
                            if tx.send(serialized).is_err() {
                                client_disconnected = true;
                                return false;
                            }
                        }
                        ChunkSSE::Done => {
                            #[cfg(feature = "enable_eval_log")]
                            {
                                let latency = start_time_clone.elapsed().as_millis();
                                log_eval_response_streaming(
                                    &request_id_clone,
                                    &req_clone,
                                    latency,
                                    n_prompt_tokens_clone,
                                    &model_name_clone,
                                );
                            }

                            let _ = tx.send("[DONE]".to_string());
                        }
                        ChunkSSE::Error(e) => {
                            let err = oai_error(e, "server_error");
                            let data = serde_json::to_string(&err).unwrap_or_default();
                            let _ = tx.send(data);
                            let _ = tx.send("[DONE]".to_string());
                        }
                    }
                    true
                },
            );

            // If the client disconnected, cancel the engine task.
            if client_disconnected {
                log::info!(
                    "[http] streaming: client disconnected, cancelling task id={}",
                    task_id
                );
                let _ = engine_for_worker.lock().unwrap().cancel_task(task_id);
            }
        });

        let s = UnboundedReceiverStream::new(rx_tokio)
            .map(|data| Ok::<Event, std::convert::Infallible>(Event::default().data(data)));
        Sse::new(s)
            .keep_alive(
                axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(30)),
            )
            .into_response()
    } else {
        // non-streaming
        // CancelGuard ensures the task is cancelled if the client disconnects
        // while the blocking task is waiting for engine replies.
        let mut guard = CancelGuard::new(engine.clone(), task_id);

        let join = tokio::task::spawn_blocking(move || {
            collect_engine_reply_to_json(
                rx,
                STREAM_TIMEOUT,
                ParserKind::Chat,
                &respond_id,
                &model_name,
                n_prompt_tokens,
                include_usage,
            )
            .map_err(|e| match e.as_str() {
                ERR_CANCELLED => (StatusCode::CONFLICT, oai_error(e, "cancelled")),
                ERR_ENGINE_SHUTDOWN => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error(e, "server_error"),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error(e, "server_error"),
                ),
            })
        });

        let result = join.await;
        guard.disarm();

        match result {
            Ok(Ok(val)) => {
                #[cfg(feature = "enable_eval_log")]
                {
                    let latency = start_time.elapsed().as_millis();
                    log_eval_response(
                        &request_id,
                        &req,
                        &val,
                        latency,
                        n_prompt_tokens,
                        &model_name,
                    );
                }

                Json(val).into_response()
            }
            Ok(Err((code, err_info))) => {
                log::debug!(
                    "[http] non-streaming: join.await returned Err: {}",
                    err_info.error.message
                );
                (code, Json(err_info)).into_response()
            }
            Err(_) => {
                log::debug!("[http] non-streaming: join.await returned JoinError (task panicked)");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(oai_error("request task failed", "server_error")),
                )
                    .into_response()
            }
        }
    }
}

#[cfg(feature = "enable_eval_log")]
fn log_eval_response(
    request_id: &str,
    req: &CreateChatCompletionRequest,
    response: &serde_json::Value,
    latency_ms: u128,
    prompt_tokens: usize,
    model_name: &str,
) {
    use async_openai::types::chat::Role;

    let mut output = String::new();
    let mut reasoning = None;
    let mut tool_calls = Vec::new();
    let mut completion_tokens = 0;
    let mut stop_reason = "unknown".to_string();

    if let Some(choices) = response
        .get(crate::llm::OUTMSG_FIELD_CHOICES)
        .and_then(|c| c.as_array())
    {
        if let Some(choice) = choices.first() {
            if let Some(message) = choice.get("message") {
                if let Some(c) = message
                    .get(crate::llm::OUTMSG_FIELD_CONTENT)
                    .and_then(|c| c.as_str())
                {
                    output = c.to_string();
                }
                if let Some(c) = message
                    .get(crate::llm::OUTMSG_FIELD_REASONING)
                    .and_then(|r| r.as_str())
                {
                    reasoning = Some(c.to_string());
                }
                if let Some(calls) = message.get(crate::llm::OUTMSG_FIELD_TOOL_CALLS) {
                    if let Some(arr) = calls.as_array() {
                        for call in arr {
                            if let Some(func) = call.get("function") {
                                let name = func
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("unknown");
                                let args = func
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                tool_calls.push(ToolCallLog {
                                    tool_name: name.to_string(),
                                    arguments: args,
                                    success: true,
                                    error: None,
                                });
                            }
                        }
                    }
                }
            }
            if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
                stop_reason = finish_reason.to_string();
            }
        }
    }

    if let Some(usage) = response.get("usage") {
        completion_tokens = usage
            .get("completion_tokens")
            .and_then(|c| c.as_u64())
            .unwrap_or(0) as usize;
    }

    let input = req
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.as_ref())
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let entry = EvalLogEntry {
        request_id: request_id.to_string(),
        timestamp: crate::helpers::format_timestamp_rfc3339(),
        role: "assistant".to_string(),
        domain: "chat".to_string(),
        source: "serve".to_string(),
        input,
        output,
        reasoning,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        latency_ms,
        prompt_tokens,
        completion_tokens,
        stop_reason,
        model: model_name.to_string(),
        is_stream: req.stream.unwrap_or(false),
        retrieval_context: None,
        expected_output: None,
    };

    if let Err(e) = get_eval_logger().log(&entry) {
        log::warn!("failed to log eval entry: {}", e);
    }
}

#[cfg(feature = "enable_eval_log")]
fn log_eval_response_streaming(
    request_id: &str,
    req: &CreateChatCompletionRequest,
    latency_ms: u128,
    prompt_tokens: usize,
    model_name: &str,
) {
    use async_openai::types::chat::Role;

    let input = req
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.as_ref())
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let entry = EvalLogEntry {
        request_id: request_id.to_string(),
        timestamp: crate::helpers::format_timestamp_rfc3339(),
        role: "assistant".to_string(),
        domain: "chat.stream".to_string(),
        source: "serve".to_string(),
        input,
        output: String::new(),
        reasoning: None,
        tool_calls: None,
        latency_ms,
        prompt_tokens,
        completion_tokens: 0,
        stop_reason: "stream_done".to_string(),
        model: model_name.to_string(),
        is_stream: true,
        retrieval_context: None,
        expected_output: None,
    };

    if let Err(e) = get_eval_logger().log(&entry) {
        log::warn!("failed to log eval entry: {}", e);
    }
}
