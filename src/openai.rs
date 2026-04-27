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
use llama_cpp_ffi::LlamaModel;
use minijinja::{Environment, context};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::engine::{Engine, EngineReply, OutputMode, StopReason, Token, now_unix_s};

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
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelInfo>,
}

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

#[derive(Deserialize)]
struct ChatCompletionsRequest {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    stream_options: Option<StreamOptions>,
    #[serde(default = "default_temperature")]
    temperature: f32,
    #[serde(default = "default_top_p")]
    top_p: f32,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default)]
    id_slot: Option<i32>,
    #[serde(default)]
    cache_prompt: bool,
}

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default)]
    content: Option<MessageContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
    Null,
}

#[derive(Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    text: Option<String>,
}

impl MessageContent {
    fn to_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => {
                let mut out = String::new();
                for p in parts {
                    if p.type_ == "text" {
                        if let Some(t) = &p.text {
                            out.push_str(t);
                        }
                    }
                }
                out
            }
            MessageContent::Null => String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamOptions {
    #[serde(default)]
    include_usage: Option<bool>,
}

fn default_temperature() -> f32 {
    1.0
}
fn default_top_p() -> f32 {
    1.0
}

#[derive(Deserialize)]
struct CompletionsRequest {
    #[serde(default)]
    model: Option<String>,
    prompt: Vec<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    echo: Option<bool>,
    #[serde(default)]
    best_of: Option<u32>,
    #[serde(default)]
    suffix: Option<String>,
    #[serde(default = "default_temperature")]
    temperature: f32,
    #[serde(default = "default_top_p")]
    top_p: f32,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default)]
    id_slot: Option<i32>,
    #[serde(default)]
    cache_prompt: bool,
}

// --- Response types ---

#[derive(Debug, Serialize)]
struct ChatCompletionsResponse {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<ChatChoice>,
    usage: Usage,
}

#[derive(Debug, Serialize)]
struct ChatChoice {
    index: u32,
    message: AssistantMessage,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct AssistantMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsChunk {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<ChatChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
struct ChatChunkChoice {
    index: u32,
    delta: ChatDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Debug, Serialize)]
struct CompletionsResponse {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<CompletionChoice>,
    usage: Usage,
}

#[derive(Debug, Serialize)]
struct CompletionChoice {
    index: u32,
    text: String,
    finish_reason: String,
}

#[derive(Debug, Serialize)]
struct CompletionChunk {
    id: String,
    object: &'static str,
    created: i64,
    model: String,
    choices: Vec<CompletionChunkChoice>,
}

#[derive(Debug, Serialize)]
struct CompletionChunkChoice {
    index: u32,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct Usage {
    prompt_tokens: usize,
    completion_tokens: usize,
    total_tokens: usize,
}

#[derive(Serialize)]
struct JinjaMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Preprocess chat template for minijinja compatibility.
fn preprocess_chat_template(src: &str) -> String {
    src.replace("content.startswith(", "startswith(content, ")
        .replace("content.endswith(", "endswith(content, ")
}

fn render_chat_template(
    template_src: &str,
    messages: &[JinjaMessage],
    eos_token: &str,
) -> Result<String, String> {
    let tmpl_str = preprocess_chat_template(template_src);
    let mut env = Environment::new();
    env.add_function("startswith", |s: String, prefix: String| -> bool {
        s.starts_with(&prefix)
    });
    env.add_function("endswith", |s: String, suffix: String| -> bool {
        s.ends_with(&suffix)
    });
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
struct AppState {
    engine: Arc<Engine>,
    model: LlamaModel,
    model_id: String,
    default_max_tokens: usize,
}

fn build_router(
    engine: Arc<Engine>,
    model: LlamaModel,
    model_id: String,
    default_max_tokens: usize,
) -> Router {
    let state = AppState {
        engine,
        model,
        model_id,
        default_max_tokens,
    };

    Router::new()
        .route("/", get(handle_root))
        .route("/health", get(handle_health))
        .route("/models", get(handle_models))
        .route("/v1/health", get(handle_health))
        .route("/v1/models", get(handle_models))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/completions", post(handle_completions))
        .with_state(state)
        .layer(CorsLayer::permissive())
}

impl Engine {
    /// Start http server with Engine (in current thread), blocks until shutdown signal.
    pub async fn http_server_with_shutdown(
        self: Arc<Self>,
        model: LlamaModel,
        model_id: String,
        default_max_tokens: usize,
        host: &str,
        port: u16,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), String> {
        eprintln!("[DEBUG http_server_with_shutdown] building router...");
        let app = build_router(self, model, model_id, default_max_tokens);
        let addr = format!("{host}:{port}");
        eprintln!("[DEBUG http_server_with_shutdown] binding to {addr}...");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        eprintln!("[DEBUG http_server_with_shutdown] listening on {addr}");
        log::info!("openai server listening on {addr}");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                eprintln!("[DEBUG http_server_with_shutdown] shutdown signal received");
                let _ = shutdown_rx.await;
            })
            .await
            .map_err(|e| format!("server error: {e}"))
    }

    /// start http server with Engine (in current thread)
    pub async fn http_server(
        self: Arc<Self>,
        model: LlamaModel,
        model_id: String,
        default_max_tokens: usize,
        host: &str,
        port: u16,
    ) -> Result<(), String> {
        let app = build_router(self, model, model_id, default_max_tokens);
        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| format!("bind failed: {e}"))?;

        log::info!("openai server listening on {addr}");
        axum::serve(listener, app)
            .await
            .map_err(|e| format!("server error: {e}"))
    }
}

//
// Handlers
//

async fn handle_root() -> impl IntoResponse {
    eprintln!("[DEBUG handle_root]");
    Json(json!({"name":"llama-js server"}))
}

async fn handle_health() -> impl IntoResponse {
    eprintln!("[DEBUG handle_health]");
    Json(HealthResponse { status: "ok" })
}

async fn handle_models(State(state): State<AppState>) -> impl IntoResponse {
    eprintln!("[DEBUG handle_models]");
    let created = now_unix_s();
    let data = vec![ModelInfo {
        id: state.model_id,
        object: "model",
        created,
        owned_by: "local",
    }];
    Json(ModelsResponse {
        object: "list",
        data,
    })
}

async fn handle_chat_completions(
    State(state): State<AppState>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    eprintln!("[DEBUG handle_chat_completions] received request");

    let raw = match payload {
        Ok(Json(v)) => v,
        Err(e) => {
            eprintln!("[DEBUG handle_chat_completions] JSON parse error: {e}");
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e.to_string(), "invalid_request_error")),
            )
                .into_response();
        }
    };

    eprintln!("[DEBUG handle_chat_completions] raw body: {}", raw);

    let mut req: ChatCompletionsRequest = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[DEBUG handle_chat_completions] deserialize error: {e}");
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e.to_string(), "invalid_request_error")),
            )
                .into_response();
        }
    };

    if req.messages.is_empty() {
        eprintln!("[DEBUG handle_chat_completions] messages is empty");
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error(
                "\"messages\" must not be empty",
                "invalid_request_error",
            )),
        )
            .into_response();
    }

    if req.max_tokens.is_none() {
        req.max_tokens = Some(state.default_max_tokens);
    }

    let is_chat = true;
    let stream = req.stream;
    let model_id = state.model_id.clone();
    let max_tokens = req.max_tokens.unwrap_or(0);
    let temperature = req.temperature;
    let top_p = req.top_p;
    let stop_words = req.stop.clone();
    let include_usage = req
        .stream_options
        .as_ref()
        .and_then(|s| s.include_usage)
        .unwrap_or(false);

    eprintln!(
        "[DEBUG handle_chat_completions] stream={}, max_tokens={}, temperature={}, top_p={}, stop_words={:?}",
        stream, max_tokens, temperature, top_p, stop_words,
    );

    // Render chat template -> tokenize prompt
    let prompt_tokens = match build_chat_prompt_tokens(&state.model, &req.messages) {
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

    eprintln!(
        "[DEBUG handle_chat_completions] prompt_tokens count={}, first_few={:?}",
        prompt_tokens.len(),
        &prompt_tokens[..prompt_tokens.len().min(10)],
    );

    let seq_hint = req
        .id_slot
        .and_then(|v| if v >= 0 { Some(v as usize) } else { None });
    let use_cache = req.cache_prompt;

    let respond_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let task_id = format!("task-{}", Uuid::new_v4().simple());
    let n_prompt_tokens = prompt_tokens.len();

    eprintln!(
        "[DEBUG handle_chat_completions] pushing task, task_id={task_id}, seq_hint={seq_hint:?}, use_cache={use_cache}"
    );

    let rx = match state.engine.push_task_with_params(
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
            eprintln!("[DEBUG handle_chat_completions] push_task_with_params error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(oai_error(e.to_string(), "server_error")),
            )
                .into_response();
        }
    };

    eprintln!("[DEBUG handle_chat_completions] task pushed, waiting for reply...");

    if stream {
        let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Bridge crossbeam Receiver -> tokio mpsc for SSE
        let _ = tokio::task::spawn_blocking(move || {
            bridge_engine_reply_to_sse(
                rx,
                tx,
                is_chat,
                &model_id,
                &respond_id,
                include_usage,
                n_prompt_tokens,
            );
        });

        let s = UnboundedReceiverStream::new(rx_tokio)
            .map(|data| Ok::<Event, Infallible>(Event::default().data(data)));

        Sse::new(s)
            .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response()
    } else {
        // Unary: collect all EngineReply, build final OAI response
        let join = tokio::task::spawn_blocking(move || {
            collect_unary_reply(rx, is_chat, &model_id, &respond_id, n_prompt_tokens)
        });

        match join.await {
            Ok(Ok(res)) => {
                eprintln!("[DEBUG handle_chat_completions] success, response: {}", res);
                Json::<serde_json::Value>(res).into_response()
            }
            Ok(Err((code, body))) => {
                eprintln!(
                    "[DEBUG handle_chat_completions] error response: code={code}, body={:?}",
                    body
                );
                (code, Json(body)).into_response()
            }
            Err(e) => {
                eprintln!("[DEBUG handle_chat_completions] join error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(oai_error("request task failed", "server_error")),
                )
                    .into_response()
            }
        }
    }
}

async fn handle_completions(
    State(state): State<AppState>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response {
    let raw = match payload {
        Ok(Json(v)) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e.to_string(), "invalid_request_error")),
            )
                .into_response();
        }
    };

    let mut req: CompletionsRequest = match serde_json::from_value(raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e.to_string(), "invalid_request_error")),
            )
                .into_response();
        }
    };

    if req.prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error("\"prompt\" is required", "invalid_request_error")),
        )
            .into_response();
    }

    if req.echo.unwrap_or(false) {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error("echo is not supported", "invalid_request_error")),
        )
            .into_response();
    }
    if req.best_of.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error(
                "best_of is not supported",
                "invalid_request_error",
            )),
        )
            .into_response();
    }
    if req.suffix.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error(
                "suffix is not supported",
                "invalid_request_error",
            )),
        )
            .into_response();
    }

    if req.max_tokens.is_none() {
        req.max_tokens = Some(state.default_max_tokens);
    }

    let is_chat = false;
    let stream = req.stream;
    let model_id = state.model_id.clone();
    let max_tokens = req.max_tokens.unwrap_or(0);
    let temperature = req.temperature;
    let top_p = req.top_p;
    let stop_words = req.stop.clone();

    // Tokenize the first prompt
    let prompt_text = req.prompt.first().cloned().unwrap_or_default();
    let prompt_tokens = match tokenize_prompt(&state.model, &prompt_text) {
        Ok(tokens) => tokens,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(oai_error(e, "invalid_request_error")),
            )
                .into_response();
        }
    };

    let seq_hint = req
        .id_slot
        .and_then(|v| if v >= 0 { Some(v as usize) } else { None });
    let use_cache = req.cache_prompt;

    let respond_id = format!("cmpl-{}", Uuid::new_v4().simple());
    let task_id = format!("task-{}", Uuid::new_v4().simple());
    let n_prompt_tokens = prompt_tokens.len();

    let rx = match state.engine.push_task_with_params(
        task_id,
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
        let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();

        let _ = tokio::task::spawn_blocking(move || {
            bridge_engine_reply_to_sse(
                rx,
                tx,
                is_chat,
                &model_id,
                &respond_id,
                false,
                n_prompt_tokens,
            );
        });

        let s = UnboundedReceiverStream::new(rx_tokio)
            .map(|data| Ok::<Event, Infallible>(Event::default().data(data)));

        Sse::new(s)
            .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response()
    } else {
        let join = tokio::task::spawn_blocking(move || {
            collect_unary_reply(rx, is_chat, &model_id, &respond_id, n_prompt_tokens)
        });

        match join.await {
            Ok(Ok(res)) => Json::<serde_json::Value>(res).into_response(),
            Ok(Err((code, body))) => (code, Json(body)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(oai_error("request task failed", "server_error")),
            )
                .into_response(),
        }
    }
}

//
// Prompt building helpers
//

fn build_chat_prompt_tokens(
    model: &LlamaModel,
    messages: &[ChatMessage],
) -> Result<Vec<Token>, String> {
    let tmpl = model.chat_template().ok_or("model has no chat template")?;
    let tmpl_str = tmpl
        .to_str()
        .map_err(|e| format!("invalid template: {e}"))?;
    let eos_token = model.eos_token_text().unwrap_or_default();

    eprintln!("[DEBUG build_chat_prompt_tokens] template: {tmpl_str}");
    eprintln!("[DEBUG build_chat_prompt_tokens] eos_token: {eos_token}");

    // Build owned message structs so the template render can borrow them.
    let backing: Vec<(String, String)> = messages
        .iter()
        .map(|m| {
            let content = m.content.as_ref().map(|c| c.to_text()).unwrap_or_default();
            eprintln!(
                "[DEBUG build_chat_prompt_tokens] msg role={}, content={}",
                m.role, content
            );
            (m.role.clone(), content)
        })
        .collect();
    let jinja_msgs: Vec<JinjaMessage<'_>> = backing
        .iter()
        .map(|(role, content)| JinjaMessage { role, content })
        .collect();

    let prompt = render_chat_template(tmpl_str, &jinja_msgs, &eos_token)?;
    eprintln!("[DEBUG build_chat_prompt_tokens] rendered prompt: {prompt}");
    let tokens = tokenize_prompt(model, &prompt)?;
    eprintln!(
        "[DEBUG build_chat_prompt_tokens] tokenized count={}",
        tokens.len()
    );
    Ok(tokens)
}

fn tokenize_prompt(model: &LlamaModel, prompt: &str) -> Result<Vec<Token>, String> {
    let c_prompt = CString::new(prompt).map_err(|_| "prompt contains NUL")?;
    let tokens = model.tokenize(&c_prompt, false, true);
    if tokens.is_empty() {
        return Err("prompt tokenized to zero tokens".to_string());
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Channel bridges: crossbeam Receiver -> HTTP response
// ---------------------------------------------------------------------------

/// Collect all EngineReply messages for a unary (non-streaming) response.
fn collect_unary_reply(
    rx: Receiver<EngineReply>,
    is_chat: bool,
    model_id: &str,
    respond_id: &str,
    n_prompt_tokens: usize,
) -> Result<serde_json::Value, (StatusCode, ErrorResponse)> {
    eprintln!("[DEBUG collect_unary_reply] waiting for replies...");
    let mut full_text = String::new();
    let mut completion_tokens = 0usize;
    let mut finish_reason;

    loop {
        match rx.recv_timeout(Duration::from_secs(300)) {
            Ok(EngineReply::Text(t)) => {
                eprintln!("[DEBUG collect_unary_reply] Text chunk: {t}");
                full_text.push_str(&t);
                completion_tokens += 1;
            }
            Ok(EngineReply::Stop(reason)) => {
                eprintln!("[DEBUG collect_unary_reply] Stop: {reason:?}");
                finish_reason = match reason {
                    StopReason::Eog => "stop".to_string(),
                    StopReason::MaxTokens => "length".to_string(),
                    StopReason::Word(_) => "stop".to_string(),
                    StopReason::PrefillOnly => "stop".to_string(),
                };
                break;
            }
            Ok(EngineReply::Cancelled) => {
                eprintln!("[DEBUG collect_unary_reply] Cancelled");
                return Err((StatusCode::CONFLICT, oai_error("cancelled", "cancelled")));
            }
            Ok(EngineReply::Error(e)) => {
                eprintln!("[DEBUG collect_unary_reply] Error: {e}");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error(e, "server_error"),
                ));
            }
            Ok(EngineReply::Shutdown) => {
                eprintln!("[DEBUG collect_unary_reply] Shutdown");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error("engine shutdown", "server_error"),
                ));
            }
            Err(e) => {
                eprintln!("[DEBUG collect_unary_reply] recv_timeout error: {e}");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    oai_error("response channel timed out", "server_error"),
                ));
            }
        }
    }

    let usage = Usage {
        prompt_tokens: n_prompt_tokens,
        completion_tokens,
        total_tokens: n_prompt_tokens + completion_tokens,
    };

    let created = now_unix_s();

    if is_chat {
        Ok(serde_json::to_value(ChatCompletionsResponse {
            id: respond_id.to_string(),
            object: "chat.completion",
            created,
            model: model_id.to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: AssistantMessage {
                    role: "assistant",
                    content: full_text,
                },
                finish_reason,
            }],
            usage,
        })
        .unwrap_or(serde_json::Value::Null))
    } else {
        Ok(serde_json::to_value(CompletionsResponse {
            id: respond_id.to_string(),
            object: "text_completion",
            created,
            model: model_id.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                text: full_text,
                finish_reason,
            }],
            usage,
        })
        .unwrap_or(serde_json::Value::Null))
    }
}

/// Bridge crossbeam Receiver to tokio mpsc for SSE streaming.
fn bridge_engine_reply_to_sse(
    rx: Receiver<EngineReply>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    is_chat: bool,
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
                if !sent_start && is_chat {
                    // First chunk: emit role delta
                    sent_start = true;
                    let first = ChatCompletionsChunk {
                        id: respond_id.to_string(),
                        object: "chat.completion.chunk",
                        created: now_unix_s(),
                        model: model_id.to_string(),
                        choices: vec![ChatChunkChoice {
                            index: 0,
                            delta: ChatDelta {
                                role: Some("assistant"),
                                content: None,
                            },
                            finish_reason: None,
                        }],
                        usage: None,
                    };
                    let _ = tx.send(serde_json::to_string(&first).unwrap_or_default());
                }

                completion_tokens += 1;

                if is_chat {
                    let chunk = ChatCompletionsChunk {
                        id: respond_id.to_string(),
                        object: "chat.completion.chunk",
                        created: now_unix_s(),
                        model: model_id.to_string(),
                        choices: vec![ChatChunkChoice {
                            index: 0,
                            delta: ChatDelta {
                                role: None,
                                content: Some(t),
                            },
                            finish_reason: None,
                        }],
                        usage: None,
                    };
                    let _ = tx.send(serde_json::to_string(&chunk).unwrap_or_default());
                } else {
                    let chunk = CompletionChunk {
                        id: respond_id.to_string(),
                        object: "text_completion",
                        created: now_unix_s(),
                        model: model_id.to_string(),
                        choices: vec![CompletionChunkChoice {
                            index: 0,
                            text: t,
                            finish_reason: None,
                        }],
                    };
                    let _ = tx.send(serde_json::to_string(&chunk).unwrap_or_default());
                }
            }
            Ok(EngineReply::Stop(reason)) => {
                let fr = match reason {
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => "stop",
                    StopReason::MaxTokens => "length",
                };

                let usage_val = if include_usage {
                    Some(Usage {
                        prompt_tokens: n_prompt_tokens,
                        completion_tokens,
                        total_tokens: n_prompt_tokens + completion_tokens,
                    })
                } else {
                    None
                };

                if is_chat {
                    let last = ChatCompletionsChunk {
                        id: respond_id.to_string(),
                        object: "chat.completion.chunk",
                        created: now_unix_s(),
                        model: model_id.to_string(),
                        choices: vec![ChatChunkChoice {
                            index: 0,
                            delta: ChatDelta {
                                role: None,
                                content: None,
                            },
                            finish_reason: Some(fr.to_string()),
                        }],
                        usage: usage_val,
                    };
                    let _ = tx.send(serde_json::to_string(&last).unwrap_or_default());
                } else {
                    let last = CompletionChunk {
                        id: respond_id.to_string(),
                        object: "text_completion",
                        created: now_unix_s(),
                        model: model_id.to_string(),
                        choices: vec![CompletionChunkChoice {
                            index: 0,
                            text: String::new(),
                            finish_reason: Some(fr.to_string()),
                        }],
                    };
                    let _ = tx.send(serde_json::to_string(&last).unwrap_or_default());
                }

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
