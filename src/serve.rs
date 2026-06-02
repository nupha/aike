use std::{
    ffi::CString,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_openai::types::chat::{CreateChatCompletionRequest, ReasoningEffort};
use axum::{
    Json, Router,
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
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[cfg(feature = "enable_eval_log")]
use crate::eval_logger::{EvalLogEntry, ToolCallLog, get_eval_logger};

use crate::{
    helpers::{
        PromptThinkPattern, apply_chat_template, detect_prompt_think_pattern, now_unix_s,
        to_lark_tool_choice,
    },
    llm::engine::{Engine, GrammarConfig, TaskConfig},
    output::{
        ChunkSSE, ParserKind, collect_engine_reply_to_json, redirect_engine_reply_to_chat_sse,
    },
};

pub(crate) const ERR_CANCELLED: &str = "cancelled";
pub(crate) const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";
pub(crate) const DEFAULT_TEMPERATURE: f32 = 0.75;
pub(crate) const DEFAULT_TOP_P: f32 = 1.0;
pub(crate) const STREAM_TIMEOUT: Duration = Duration::from_secs(300);

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

#[derive(Clone)]
#[repr(transparent)]
pub(crate) struct AppState {
    pub(crate) engine: Arc<Mutex<Engine>>,
}

/// Start http server with Engine (in current thread), blocks until shutdown signal.
pub async fn http_serve(
    engine: Arc<Mutex<Engine>>,
    host: &str,
    port: u16,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
    let state = AppState { engine };

    let app = Router::new()
        .route("/", get(handle_root))
        .route("/health", get(handle_health))
        .route("/models", get(handle_models))
        .route("/v1/health", get(handle_health))
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/responses", post(crate::responses::handle_responses))
        .with_state(state)
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(|e| format!("failed to bind {host}:{port}: {e}"))?;

    log::info!("Engine http server listening on {host}:{port}");

    axum::serve(listener, app)
        .with_graceful_shutdown(async { drop(shutdown_rx.await) })
        .await
        .map_err(|e| format!("server error: {e}"))
}

async fn handle_root() -> impl IntoResponse {
    Json(json!({"name":"llm server"}))
}

async fn handle_health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    let data = vec![ModelInfo {
        id: state.engine.lock().unwrap().model_name().to_string(),
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
    State(state): State<AppState>,
    Json(req): Json<CreateChatCompletionRequest>,
) -> Response {
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

    let parse_reasoning: bool;
    let default_max_tokens: usize;
    let model_name: String;
    let output_prepend: Vec<i32>;

    // Build prompt tokens
    let prompt_tokens = {
        let engine = state.engine.lock().unwrap();
        let model = engine.model();

        parse_reasoning = engine.parse_reasoning();
        default_max_tokens = engine.default_max_tokens();
        model_name = engine.model_name().to_string();

        match apply_chat_template(
            model,
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

    let mut task_config = TaskConfig::new()
        .max_tokens(
            req.max_completion_tokens
                .unwrap_or(default_max_tokens as u32) as usize,
        )
        .temperature(req.temperature.unwrap_or(DEFAULT_TEMPERATURE))
        .top_p(req.top_p.unwrap_or(DEFAULT_TOP_P))
        .enable_output_parser(parse_reasoning)
        .output_stream(is_stream);

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
            task_config = task_config.grammar(GrammarConfig::new(
                state.engine.lock().unwrap().model(),
                tools,
                tc,
            ));
        }
    }

    let n_prompt_tokens = prompt_tokens.len();
    let respond_id = format!("chatcmpl-{}", Uuid::new_v4().simple());

    let rx = match state.engine.lock().unwrap().push_task(
        prompt_tokens,
        task_config,
        output_prepend,
        None,
    ) {
        Ok(rx) => rx,
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

        tokio::task::spawn_blocking(move || {
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
                            let _ = tx.send(serialized);
                        }
                        ChunkSSE::Done => {
                            #[cfg(feature = "enable_eval_log")]
                            {
                                // 流式结束，记录评估日志
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

        match join.await {
            Ok(Ok(val)) => {
                #[cfg(feature = "enable_eval_log")]
                {
                    // 记录评估日志
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
            Ok(Err((code, err_info))) => (code, Json(err_info)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(oai_error("request task failed", "server_error")),
            )
                .into_response(),
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

    // 从响应中提取内容
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

    // 流式响应的日志记录（简化版，因为没有完整响应体）
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
        output: String::new(), // 流式响应在这里无法获取完整输出
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
