use std::{
    ffi::CString,
    sync::{Arc, Mutex},
};

use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, InputItem, InputParam, MessageType, Role,
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response, Sse, sse::Event},
};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::{
    helpers::{PromptThinkPattern, apply_chat_template, detect_prompt_think_pattern},
    llm::engine::{Engine, EngineReply, GrammarConfig, TaskConfig},
    output::{
        ChunkSSE, ParserKind, collect_engine_reply_to_json, redirect_engine_reply_to_responses_sse,
    },
    responses::stream::sse_event,
    serve::{
        AppState, DEFAULT_TEMPERATURE, DEFAULT_TOP_P, ERR_CANCELLED, ERR_ENGINE_SHUTDOWN,
        STREAM_TIMEOUT, oai_error,
    },
};

use super::convert::{input_item_to_chat_message, responses_tools_to_chat_tools};

pub async fn handle_responses(
    State(state): State<AppState>,
    Json(req): Json<CreateResponse>,
) -> Response {
    // Quick check: if the engine worker has crashed, reject immediately.
    if !state.engine.lock().unwrap().is_alive() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(oai_error("engine worker has crashed", "server_error")),
        )
            .into_response();
    }

    let input_items = match &req.input {
        InputParam::Text(t) => {
            vec![InputItem::EasyMessage(EasyInputMessage {
                r#type: MessageType::Message,
                role: Role::User,
                content: EasyInputContent::Text(t.clone()),
                phase: None,
            })]
        }
        InputParam::Items(items) => items.clone(),
    };
    let messages: Vec<_> = input_items
        .iter()
        .filter_map(input_item_to_chat_message)
        .collect();

    if messages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(oai_error(
                "\"input\" must contain at least one valid message",
                "invalid_request_error",
            )),
        )
            .into_response();
    }

    let is_stream = req.stream.unwrap_or(false);
    let enable_thinking = req.reasoning.is_some();
    let tools = req.tools.as_ref().map(|t| responses_tools_to_chat_tools(t));
    let tool_choice = None;
    let output_prepend: Vec<i32>;

    let n_prompt_tokens;
    let respond_id;
    let model_name;
    let rx;
    let task_id;
    let max_tokens;
    let temperature;
    let top_p;

    // Lock engine in a block so MutexGuard is dropped before any await
    {
        let engine = state.engine.lock().unwrap();

        max_tokens = req
            .max_output_tokens
            .unwrap_or(engine.default_max_tokens() as u32) as usize;
        temperature = req.temperature.unwrap_or(DEFAULT_TEMPERATURE);
        top_p = req.top_p.unwrap_or(DEFAULT_TOP_P);

        let prompt_tokens = match apply_chat_template(
            engine.model(),
            &messages,
            tools.as_deref(),
            tool_choice.as_ref(),
            None,
            req.reasoning
                .as_ref()
                .and_then(|r| r.effort.clone())
                .unwrap_or_default(),
            None,
        ) {
            Ok(prompt) => {
                output_prepend = if enable_thinking && engine.parse_reasoning() {
                    let tags = engine.model().arch().think_boundaries();
                    if detect_prompt_think_pattern(&prompt, tags.0, tags.1)
                        == PromptThinkPattern::Open
                    {
                        engine.model().tokenize(
                            CString::new(tags.0).unwrap().as_c_str(),
                            false,
                            false,
                        )
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                engine
                    .model()
                    .tokenize(CString::new(prompt).unwrap().as_c_str(), false, true)
            }
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(oai_error(e, "invalid_request_error")),
                )
                    .into_response();
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
            .max_tokens(max_tokens)
            .temperature(temperature)
            .top_p(top_p)
            .output_stream(is_stream);

        if let Some(tools) = tools.as_ref().filter(|t| !t.is_empty()) {
            task_config = task_config.grammar(GrammarConfig::new(
                state.engine.lock().unwrap().model(),
                tools,
                llama_cpp_ffi::ToolChoice::Auto,
            ));
        }

        n_prompt_tokens = prompt_tokens.len();
        respond_id = format!("res-{}", Uuid::new_v4().simple());
        model_name = engine.model_name().to_string();

        // Save task_id before task_config is moved into push_task.
        task_id = task_config.task_id;

        rx = match engine.push_task(prompt_tokens, task_config, output_prepend, None) {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(oai_error(e.to_string(), "server_error")),
                )
                    .into_response();
            }
        };
    } // engine (MutexGuard) is dropped here, before any .await

    // CancelGuard ensures the task is cancelled if the client disconnects.
    let mut guard = crate::serve::CancelGuard::new(state.engine.clone(), task_id);

    if is_stream {
        // Don't disarm the guard — it ensures the task is cancelled immediately
        // when the client disconnects (future dropped), even if spawn_blocking
        // is blocked on rx.recv_timeout waiting for the next engine token.
        handle_responses_stream(
            rx,
            respond_id,
            model_name,
            n_prompt_tokens,
            state.engine.clone(),
            task_id,
        )
    } else {
        // non-stream
        let join = tokio::task::spawn_blocking(move || {
            collect_engine_reply_to_json(
                rx,
                STREAM_TIMEOUT,
                ParserKind::Responses,
                &respond_id,
                &model_name,
                n_prompt_tokens,
                true,
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
        // Disarm the guard on any completed outcome.
        // If the future was dropped (client disconnected), guard will fire instead.
        guard.disarm();

        match result {
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

pub(crate) fn handle_responses_stream(
    rx: crossbeam::channel::Receiver<EngineReply>,
    respond_id: String,
    model_name: String,
    n_prompt_tokens: usize,
    engine: Arc<Mutex<Engine>>,
    task_id: usize,
) -> Response {
    let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::task::spawn_blocking(move || {
        let mut client_disconnected = false;

        redirect_engine_reply_to_responses_sse(
            rx,
            STREAM_TIMEOUT,
            &respond_id,
            &model_name,
            n_prompt_tokens,
            true,
            |chunk| {
                match chunk {
                    ChunkSSE::Chunk(data) => {
                        let event_type = data["type"]
                            .as_str()
                            .unwrap_or("response.output_text.delta")
                            .to_owned();
                        let data_str = serde_json::to_string(&data).unwrap_or_default();
                        drop(data);
                        let msg = format!("event: {event_type}\ndata: {data_str}\n\n");
                        if tx.send(msg).is_err() {
                            client_disconnected = true;
                            return false;
                        }
                    }
                    ChunkSSE::Done => {
                        let _ = tx.send("[DONE]".to_string());
                    }
                    ChunkSSE::Error(e) => {
                        let err = serde_json::json!({
                            "type": "error",
                            "error": {
                                "message": e,
                                "type": "server_error"
                            }
                        });
                        let _ = tx.send(sse_event("error", err));
                    }
                }
                true
            },
        );

        if client_disconnected {
            log::info!(
                "[responses] streaming: client disconnected, cancelling task id={}",
                task_id
            );
            let _ = engine.lock().unwrap().cancel_task(task_id);
        }
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx_tokio)
        .map(|data| Ok::<_, std::convert::Infallible>(Event::default().data(data)));

    Sse::new(stream).into_response()
}
