use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_openai::types::chat::CreateChatCompletionRequest;
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

use crate::{
    chat_response::{ChatResponseParser, ChatStreamItem},
    engine::{Engine, GrammarConfig, OutputMode, TaskConfig},
    helpers::{build_chat_prompt_tokens, compile_tools_grammar, now_unix_s, to_lark_tool_choice},
};

const ERR_CANCELLED: &str = "cancelled";
const ERR_ENGINE_SHUTDOWN: &str = "engine shutdown";
const DEFAULT_TEMPERATURE: f32 = 0.75;
const DEFAULT_TOP_P: f32 = 1.0;
const STREAM_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: OpenAiError,
}

#[derive(Debug, Serialize)]
struct OpenAiError {
    message: String,
    #[serde(rename = "type")]
    type_: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

fn oai_error(message: impl Into<String>, type_: impl Into<String>) -> ErrorResponse {
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
    engine: Arc<Mutex<Engine>>,
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
    let messages = &req.messages;
    let tools = req.tools.clone();
    let tool_choice = req.tool_choice.clone();
    let parallel_tool_calls = req.parallel_tool_calls;
    let reasoning_effort = req.reasoning_effort;
    let response_format = req.response_format;
    let max_tokens =
        req.max_completion_tokens
            .unwrap_or(state.engine.lock().unwrap().default_max_tokens() as u32) as usize;
    let temperature = req.temperature.unwrap_or(DEFAULT_TEMPERATURE);
    let top_p = req.top_p.unwrap_or(DEFAULT_TOP_P);
    let stop = req.stop.clone();
    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|o| o.include_usage)
        .unwrap_or(false);

    let prompt_tokens = match build_chat_prompt_tokens(
        state.engine.lock().unwrap().model(),
        messages,
        tools.as_deref(),
        tool_choice.as_ref(),
        parallel_tool_calls,
        reasoning_effort.as_ref(),
        response_format.as_ref(),
    ) {
        Ok(tokens) => tokens,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e, "invalid_request_error")),
            )
                .into_response();
        }
    };

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
            async_openai::types::chat::StopConfiguration::String(s) => vec![s.clone()],
            async_openai::types::chat::StopConfiguration::StringArray(arr) => arr.clone(),
        };
        if !words.is_empty() {
            task_config = task_config.stop_words(words);
        }
    }

    if let Some(tools) = tools.as_ref().filter(|t| !t.is_empty()) {
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

    let rx = match state.engine.lock().unwrap().push_task_with_params(
        prompt_tokens,
        task_config,
        None,
        false,
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

    let mut parser =
        ChatResponseParser::new(reasoning_effort.is_some(), n_prompt_tokens, include_usage);
    parser.set_respond_id(respond_id.clone());
    parser.set_model_name(model_name.clone());

    if is_stream {
        // streaming
        let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();
        tokio::task::spawn_blocking(move || {
            parser.parse_stream(rx, STREAM_TIMEOUT, |item| {
                match item {
                    ChatStreamItem::Done => {
                        let _ = tx.send("[DONE]".to_string());
                    }
                    ChatStreamItem::Error(e) => {
                        let err = oai_error(e, "server_error");
                        let data = serde_json::to_string(&err).unwrap_or_default();
                        let _ = tx.send(data);
                        let _ = tx.send("[DONE]".to_string());
                    }
                    _ => {
                        let chunk = item.into_chunk(&respond_id, &model_name, now_unix_s() as u32);
                        let data = serde_json::to_string(&chunk).unwrap_or_default();
                        let _ = tx.send(data);
                    }
                }
                true
            });
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
            parser
                .parse_all(rx, STREAM_TIMEOUT)
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
            Ok(Ok(val)) => Json(val).into_response(),
            Ok(Err((code, err_info))) => (code, Json(err_info)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(oai_error("request task failed", "server_error")),
            )
                .into_response(),
        }
    }
}
