use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use futures_util::StreamExt;
use liter_llm::{ChatCompletionRequest, types::common::StopSequence};
use llama_cpp_ffi::{LlamaModel, ToolChoice};
use serde::Serialize;
use serde_json::json;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::{
    class_llama_engine::{
        ChatStreamItem, ERR_CANCELLED, ERR_ENGINE_SHUTDOWN, bridge_chat_stream,
        collect_chat_response,
    },
    engine::{Engine, GrammarConfig, OutputMode, TaskConfig, Token},
    helpers::{build_chat_prompt_tokens, compile_tools_grammar, now_unix_s, to_lark_tool_choice},
};

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

const DEFAULT_TEMPERATURE: f64 = 0.75;
const DEFAULT_TOP_P: f64 = 1.0;

#[derive(Clone)]
#[repr(transparent)]
struct AppState {
    engine: Arc<Mutex<Engine>>,
}

impl Engine {
    fn build_router(engine: Arc<Mutex<Self>>) -> Router {
        Router::new()
            .route("/", get(handle_root))
            .route("/health", get(handle_health))
            .route("/models", get(handle_models))
            .route("/v1/health", get(handle_health))
            .route("/v1/models", get(handle_models))
            .route("/v1/chat/completions", post(handle_chat_completions))
            .with_state(AppState { engine })
            .layer(CorsLayer::permissive())
    }

    /// Start http server with Engine (in current thread), blocks until shutdown signal.
    pub async fn http_serve(
        engine: Arc<Mutex<Self>>,
        host: &str,
        port: u16,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let router = Self::build_router(engine.clone());

        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        log::info!("Engine http server listening on {addr}");

        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                eprintln!("[DEBUG http_server_with_shutdown] shutdown signal received");
            })
            .await
            .map_err(|e| format!("server error: {e}"))
    }
}

//
// Handlers
//

async fn handle_root() -> impl IntoResponse {
    Json(json!({"name":"llm server"}))
}

async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
    }))
}

async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    let data = vec![ModelInfo {
        id: state.engine.lock().unwrap().model_name().to_string(),
        object: "model",
        created: now_unix_s(),
        owned_by: "local",
    }];

    Json(serde_json::json!({
        "object": "list",
        "data": data,
    }))
}

async fn handle_chat_completions(
    State(state): State<AppState>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let mut raw = match payload {
        Ok(Json(v)) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e.to_string(), "invalid_request_error")),
            )
                .into_response();
        }
    };

    // Extract server-specific fields before deserializing (ChatCompletionRequest uses deny_unknown_fields)
    let id_slot = raw
        .get("id_slot")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32);
    let cache_prompt = raw
        .get("cache_prompt")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Strip unknown fields so ChatCompletionRequest won't reject them
    if let Some(obj) = raw.as_object_mut() {
        obj.remove("id_slot");
        obj.remove("cache_prompt");
    }

    // parse request
    let req: ChatCompletionRequest = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e.to_string(), "invalid_request_error")),
            )
                .into_response();
        }
    };

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

    let stream = req.stream.unwrap_or(false);
    let temperature = req.temperature.unwrap_or(DEFAULT_TEMPERATURE) as f32;
    let top_p = req.top_p.unwrap_or(DEFAULT_TOP_P) as f32;
    let stop_words: Vec<String> = match req.stop {
        Some(StopSequence::Single(ref s)) => vec![s.clone()],
        Some(StopSequence::Multiple(ref v)) => v.clone(),
        None => vec![],
    };
    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|s| s.include_usage)
        .unwrap_or(false);

    let max_tokens =
        req.max_tokens
            .unwrap_or(state.engine.lock().unwrap().default_max_tokens() as u64) as usize;

    let prompt_tokens =
        match build_chat_prompt_tokens_from_request(state.engine.lock().unwrap().model(), &req) {
            Ok(tokens) => tokens,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(oai_error(e, "invalid_request_error")),
                )
                    .into_response();
            }
        };

    let seq_hint = id_slot.and_then(|v| if v >= 0 { Some(v as usize) } else { None });
    let use_cache = cache_prompt;

    let respond_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let n_prompt_tokens = prompt_tokens.len();

    let mut task_config = TaskConfig::new()
        .stop_words(stop_words)
        .max_tokens(max_tokens)
        .temperature(temperature)
        .top_p(top_p)
        .output_mode(if stream {
            OutputMode::Continuous
        } else {
            OutputMode::OneShot
        });

    if let Some(tools) = req
        .tools
        .as_ref()
        .and_then(|tools| if tools.is_empty() { None } else { Some(tools) })
    {
        // build grammar for tools
        let tool_choice = req
            .tool_choice
            .as_ref()
            .map(to_lark_tool_choice)
            .unwrap_or(ToolChoice::Auto);

        if tool_choice != ToolChoice::None {
            task_config = task_config.grammar(GrammarConfig {
                lark_grammar: compile_tools_grammar(tools, tool_choice),
                tool_choice,
            });
        }
    }

    let engine_rx = match state.engine.lock().unwrap().push_task_with_params(
        prompt_tokens,
        task_config,
        seq_hint,
        use_cache,
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

    if stream {
        // Bridge crossbeam Receiver -> tokio mpsc for SSE
        let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();
        let model_id = state.engine.lock().unwrap().model_name().to_string();
        tokio::task::spawn_blocking(move || {
            bridge_chat_stream(
                engine_rx,
                std::time::Duration::from_secs(300),
                &respond_id,
                &model_id,
                include_usage,
                n_prompt_tokens,
                |item| {
                    match item {
                        ChatStreamItem::Chunk(chunk) => {
                            let _ = tx.send(serde_json::to_string(&chunk).unwrap_or_default());
                        }
                        ChatStreamItem::Done => {
                            let _ = tx.send("[DONE]".to_string());
                        }
                        ChatStreamItem::Error(e) => {
                            let err = oai_error(e, "server_error");
                            let _ = tx.send(serde_json::to_string(&err).unwrap_or_default());
                            let _ = tx.send("[DONE]".to_string());
                        }
                    }
                    true
                },
            );
        });

        let s = UnboundedReceiverStream::new(rx_tokio)
            .map(|data| Ok::<Event, Infallible>(Event::default().data(data)));
        Sse::new(s)
            .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(30)))
            .into_response()
    } else {
        // Unary: collect all EngineReply, build final response
        let join = tokio::task::spawn_blocking({
            let model_id = state.engine.lock().unwrap().model_name().to_string();

            move || {
                collect_chat_response(
                    engine_rx,
                    std::time::Duration::from_secs(300),
                    &respond_id,
                    &model_id,
                    include_usage,
                    n_prompt_tokens,
                )
                .map_err(|e| match e.as_str() {
                    ERR_CANCELLED => (StatusCode::CONFLICT, oai_error(ERR_CANCELLED, "cancelled")),
                    ERR_ENGINE_SHUTDOWN => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        oai_error(ERR_ENGINE_SHUTDOWN, "server_error"),
                    ),
                    _ => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        oai_error(e, "server_error"),
                    ),
                })
            }
        });

        match join.await {
            Ok(Ok(res)) => Json::<serde_json::Value>(res).into_response(),
            Ok(Err((code, err_info))) => (code, Json(err_info)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(oai_error("request task failed", "server_error")),
            )
                .into_response(),
        }
    }
}

fn build_chat_prompt_tokens_from_request(
    model: &LlamaModel,
    req: &ChatCompletionRequest,
) -> Result<Vec<Token>, String> {
    debug_assert!(!req.messages.is_empty());

    // Delegate to shared build_chat_prompt_tokens
    build_chat_prompt_tokens(
        model,
        &req.messages,
        req.tools.as_deref(),
        req.tool_choice.as_ref(),
        req.parallel_tool_calls,
        req.reasoning_effort.as_ref(),
        req.response_format.as_ref(),
    )
}
