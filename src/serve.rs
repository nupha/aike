use std::{convert::Infallible, ffi::CString, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use crossbeam::channel::Receiver;
use futures_util::StreamExt;
use liter_llm::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Choice, FinishReason,
    StreamChoice, StreamDelta,
    types::common::{AssistantMessage, ContentPart, Message, StopSequence, Usage, UserContent},
};
use llama_cpp_ffi::LlamaModel;
use minijinja::context;
use serde::Serialize;
use serde_json::json;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::{
    engine::{Engine, EngineReply, OutputMode, StopReason, Token, now_unix_s},
    helpers::{JinjaMessage, create_minijinja_env, preprocess_chat_template},
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

fn render_chat_template(
    tmpl: &str,
    messages: &[JinjaMessage],
    eos_token: &str,
) -> Result<String, String> {
    let tmpl_str = preprocess_chat_template(tmpl);
    let mut env = create_minijinja_env();
    env.add_template("chat", &tmpl_str)
        .map_err(|e| format!("invalid chat template: {e}"))?;
    let tmpl = env.get_template("chat").map_err(|e| format!("{e}"))?;
    let ctx = context! {
        messages => messages,
        tools => None::<Vec<()>>,
        add_generation_prompt => true,
        enable_thinking => Some(false),
        add_vision_id => false,
        eos_token => eos_token,
    };
    tmpl.render(ctx).map_err(|e| format!("{e}"))
}

#[derive(Clone)]
#[repr(transparent)]
struct AppState {
    engine: Arc<Engine>,
}

impl Engine {
    fn build_router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", get(handle_root))
            .route("/health", get(handle_health))
            .route("/models", get(handle_models))
            .route("/v1/health", get(handle_health))
            .route("/v1/models", get(handle_models))
            .route("/v1/chat/completions", post(handle_chat_completions))
            .with_state(AppState { engine: self })
            .layer(CorsLayer::permissive())
    }

    /// Start http server with Engine (in current thread), blocks until shutdown signal.
    pub async fn http_serve(
        self: Arc<Self>,
        host: &str,
        port: u16,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), String> {
        let app = self.build_router();

        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        log::info!("Engine http server listening on {addr}");

        axum::serve(listener, app)
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
        id: state.engine.model_name().to_string(),
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
        Some(StopSequence::Single(s)) => vec![s],
        Some(StopSequence::Multiple(v)) => v,
        None => vec![],
    };
    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|s| s.include_usage)
        .unwrap_or(false);
    let max_tokens = req
        .max_tokens
        .unwrap_or(state.engine.default_max_tokens() as u64) as usize;

    let prompt_tokens =
        match build_chat_prompt_tokens(state.engine.llm_ctx().model(), &req.messages) {
            Ok(tokens) => tokens,
            Err(e) => {
                eprintln!("[DEBUG handle_chat_completions] build_chat_prompt_tokens error: {e}");
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
    let task_id = format!("task-{}", Uuid::new_v4().simple());
    let n_prompt_tokens = prompt_tokens.len();

    let engine_rx = match state.engine.push_task_with_params(
        task_id.clone(),
        prompt_tokens,
        stop_words,
        if stream {
            OutputMode::Continuous
        } else {
            OutputMode::OneShot
        },
        seq_hint,
        use_cache,
        max_tokens,
        temperature,
        top_p,
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

    if stream {
        // Bridge crossbeam Receiver -> tokio mpsc for SSE
        let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();
        let _ = tokio::task::spawn_blocking(move || {
            bridge_engine_reply_to_sse(
                engine_rx,
                tx,
                state.engine.model_name(),
                &respond_id,
                include_usage,
                n_prompt_tokens,
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
            let model_id = state.engine.model_name().to_string();
            move || collect_unary_reply(engine_rx, &model_id, &respond_id, n_prompt_tokens)
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

fn build_chat_prompt_tokens(
    model: &LlamaModel,
    messages: &[Message],
) -> Result<Vec<Token>, String> {
    let tmpl = model.chat_template().ok_or("model has no chat template")?;
    let tmpl = tmpl
        .to_str()
        .map_err(|e| format!("invalid template: {e}"))?;
    let eos_token = model.eos_token_text().unwrap_or_default();

    // Convert liter_llm::Message to (role, content) pairs for template rendering
    let backing: Vec<(String, String)> = messages.iter().map(message_to_role_content).collect();

    let jinja_msgs: Vec<JinjaMessage> = backing
        .into_iter()
        .map(|(role, content)| JinjaMessage { role, content })
        .collect();

    let prompt = render_chat_template(tmpl, &jinja_msgs, &eos_token)?;
    let tokens = tokenize_prompt(model, &prompt)?;
    Ok(tokens)
}

/// Extract (role, content_text) from a liter_llm::Message for template rendering.
fn message_to_role_content(msg: &Message) -> (String, String) {
    let role = match msg {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::Tool(_) => "tool",
        Message::Developer(_) => "developer",
        Message::Function(_) => "function",
    };
    let content = match msg {
        Message::System(m) => m.content.clone(),
        Message::User(m) => match &m.content {
            UserContent::Text(s) => s.clone(),
            UserContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        },
        Message::Assistant(m) => m.content.clone().unwrap_or_default(),
        Message::Tool(m) => m.content.clone(),
        Message::Developer(m) => m.content.clone(),
        Message::Function(m) => m.content.clone(),
    };
    (role.to_string(), content)
}

fn tokenize_prompt(model: &LlamaModel, prompt: &str) -> Result<Vec<Token>, String> {
    let c_prompt = CString::new(prompt).map_err(|_| "prompt contains NUL")?;
    let tokens = model.tokenize(&c_prompt, false, true);
    if tokens.is_empty() {
        return Err("prompt tokenized to zero tokens".to_string());
    }
    Ok(tokens)
}

/// Collect all EngineReply messages for a unary (non-streaming) response.
fn collect_unary_reply(
    rx: Receiver<EngineReply>,
    model_id: &str,
    respond_id: &str,
    n_prompt_tokens: usize,
) -> Result<serde_json::Value, (StatusCode, ErrorResponse)> {
    let mut full_text = String::new();
    let mut completion_tokens = 0usize;
    let finish_reason;

    loop {
        match rx.recv_timeout(Duration::from_secs(300)) {
            Ok(EngineReply::Text(t)) => {
                full_text.push_str(&t);
                completion_tokens += 1;
            }
            Ok(EngineReply::Stop(reason)) => {
                finish_reason = match reason {
                    StopReason::MaxTokens => FinishReason::Length,
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                        FinishReason::Stop
                    }
                };
                break;
            }
            Ok(EngineReply::Cancelled) => {
                return Err((StatusCode::CONFLICT, oai_error("cancelled", "cancelled")));
            }
            Ok(EngineReply::Error(e)) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error(e, "server_error"),
                ));
            }
            Ok(EngineReply::Shutdown) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error("engine shutdown", "server_error"),
                ));
            }
            Err(_) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error("response channel timed out", "server_error"),
                ));
            }
        }
    }

    Ok(serde_json::to_value(ChatCompletionResponse {
        id: respond_id.to_string(),
        object: "chat.completion".to_string(),
        created: now_unix_s() as u64,
        model: model_id.to_string(),
        choices: vec![Choice {
            index: 0,
            message: AssistantMessage {
                content: Some(full_text),
                ..Default::default()
            },
            finish_reason: Some(finish_reason),
        }],
        usage: Some(Usage {
            prompt_tokens: n_prompt_tokens as u64,
            completion_tokens: completion_tokens as u64,
            total_tokens: (n_prompt_tokens + completion_tokens) as u64,
        }),
        ..Default::default()
    })
    .unwrap_or(serde_json::Value::Null))
}

/// Bridge crossbeam Receiver to tokio mpsc for SSE streaming.
fn bridge_engine_reply_to_sse(
    rx: Receiver<EngineReply>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    model_id: &str,
    respond_id: &str,
    include_usage: bool,
    n_prompt_tokens: usize,
) {
    let mut completion_tokens = 0usize;
    let mut sent_start = false;

    loop {
        match rx.recv_timeout(Duration::from_secs(300)) {
            Ok(EngineReply::Text(t)) => {
                let created = now_unix_s() as u64;

                if !sent_start {
                    // First chunk: emit role delta
                    sent_start = true;

                    let first = ChatCompletionChunk {
                        id: respond_id.to_string(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model_id.to_string(),
                        choices: vec![StreamChoice {
                            index: 0,
                            delta: StreamDelta {
                                role: Some("assistant".to_string()),
                                content: None,
                                ..Default::default()
                            },
                            finish_reason: None,
                        }],
                        ..Default::default()
                    };
                    let _ = tx.send(serde_json::to_string(&first).unwrap_or_default());
                }

                completion_tokens += 1;

                let chunk = ChatCompletionChunk {
                    id: respond_id.to_string(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model_id.to_string(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: StreamDelta {
                            role: None,
                            content: Some(t),
                            ..Default::default()
                        },
                        finish_reason: None,
                    }],
                    ..Default::default()
                };
                let _ = tx.send(serde_json::to_string(&chunk).unwrap_or_default());
            }
            Ok(EngineReply::Stop(reason)) => {
                let fr = match reason {
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                        FinishReason::Stop
                    }
                    StopReason::MaxTokens => FinishReason::Length,
                };

                let usage = if include_usage {
                    Some(Usage {
                        prompt_tokens: n_prompt_tokens as u64,
                        completion_tokens: completion_tokens as u64,
                        total_tokens: (n_prompt_tokens + completion_tokens) as u64,
                    })
                } else {
                    None
                };

                let last = ChatCompletionChunk {
                    id: respond_id.to_string(),
                    object: "chat.completion.chunk".to_string(),
                    created: now_unix_s() as u64,
                    model: model_id.to_string(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: StreamDelta::default(),
                        finish_reason: Some(fr),
                    }],
                    usage,
                    ..Default::default()
                };
                let _ = tx.send(serde_json::to_string(&last).unwrap_or_default());

                let _ = tx.send("[DONE]".to_string());
                break;
            }
            Ok(EngineReply::Cancelled) => {
                let _ = tx.send(
                    serde_json::to_string(&oai_error("cancelled", "cancelled")).unwrap_or_default(),
                );
                let _ = tx.send("[DONE]".to_string());
                break;
            }
            Ok(EngineReply::Error(e)) => {
                let _ = tx
                    .send(serde_json::to_string(&oai_error(e, "server_error")).unwrap_or_default());
                let _ = tx.send("[DONE]".to_string());
                break;
            }
            Ok(EngineReply::Shutdown) => {
                let _ = tx.send(
                    serde_json::to_string(&oai_error("engine shutdown", "server_error"))
                        .unwrap_or_default(),
                );
                let _ = tx.send("[DONE]".to_string());
                break;
            }
            Err(_) => {
                let _ = tx.send(
                    serde_json::to_string(&oai_error("channel timed out", "server_error"))
                        .unwrap_or_default(),
                );
                let _ = tx.send("[DONE]".to_string());
                break;
            }
        }
    }
}
