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
    engine::{EngineReply, GrammarConfig, OutputMode, TaskConfig},
    helpers::{build_prompt_tokens, compile_tools_grammar},
    output::{LlmOutputParser, OutputChunk, OutputPart, ParserKind},
    responses::{
        response_builder::{
            build_content_part_added_event, build_output_item_added_event,
            build_reasoning_delta_event, build_response_completed_event,
            build_response_created_event, build_text_delta_event,
        },
        stream::sse_event,
    },
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
    let enalbe_thinking = req.reasoning.is_some();
    let tools = req.tools.as_ref().map(|t| responses_tools_to_chat_tools(t));
    let tool_choice = None;

    let n_prompt_tokens;
    let respond_id;
    let model_name;
    let rx;
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

        let prompt_tokens = match build_prompt_tokens(
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

        if let Some(tools) = tools.as_ref().filter(|t| !t.is_empty()) {
            let tc = llama_cpp_ffi::ToolChoice::Auto;
            task_config = task_config.grammar(GrammarConfig {
                lark_grammar: compile_tools_grammar(tools, tc, ("<tool_call>", "</tool_call>")),
                tool_choice: tc,
            });
        }

        n_prompt_tokens = prompt_tokens.len();
        respond_id = format!("res-{}", Uuid::new_v4().simple());
        model_name = engine.model_name().to_string();

        rx = match engine.push_task_with_params(prompt_tokens, task_config, None, false) {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(oai_error(e.to_string(), "server_error")),
                )
                    .into_response();
            }
        };
    }

    let mut parser = LlmOutputParser::new(true, enalbe_thinking);

    parser
        .set_respond_id(respond_id.clone())
        .set_model_name(model_name.clone())
        .set_n_prompt_tokens(n_prompt_tokens);

    if is_stream {
        handle_responses_stream(rx, parser, respond_id, model_name)
    } else {
        // non-stream
        let join = tokio::task::spawn_blocking(move || {
            parser
                .parse_all(rx, STREAM_TIMEOUT, ParserKind::Responses)
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

pub(crate) fn handle_responses_stream(
    rx: crossbeam::channel::Receiver<EngineReply>,
    parser: LlmOutputParser,
    respond_id: String,
    model_name: String,
) -> Response {
    let (tx, rx_tokio) = tokio::sync::mpsc::unbounded_channel::<String>();

    let n_prompt_tokens = { parser.n_prompt_tokens() };

    tokio::task::spawn_blocking(move || {
        let mut text_buffer = String::new();
        let mut reasoning_buffer = String::new();
        let mut text_started = false;
        let mut reasoning_started = false;
        let mut output_index: u32 = 0;
        let mut content_index: u32 = 0;
        let mut seq: u64 = 0;

        // Emit response.created
        let created = build_response_created_event(&respond_id, &model_name);
        let _ = tx.send(sse_event("response.created", created));

        parser.parse_partial(rx, STREAM_TIMEOUT, |item, _nth| {
            match item {
                OutputChunk::Part(part) => match part {
                    OutputPart::Text(t) => {
                        if !text_started {
                            text_started = true;

                            let item_added = build_output_item_added_event(seq, output_index);
                            seq += 1;
                            let _ = tx.send(sse_event("response.output_item.added", item_added));

                            let part_added =
                                build_content_part_added_event(seq, output_index, content_index);
                            seq += 1;
                            let _ = tx.send(sse_event("response.content_part.added", part_added));
                        }

                        text_buffer.push_str(&t);
                        let delta = build_text_delta_event(seq, output_index, content_index, &t);
                        seq += 1;
                        let _ = tx.send(sse_event("response.output_text.delta", delta));
                    }
                    OutputPart::Reasoning(r) => {
                        if !reasoning_started {
                            reasoning_started = true;
                        }
                        reasoning_buffer.push_str(&r);
                        let delta = build_reasoning_delta_event(seq, output_index, &r);
                        seq += 1;
                        let _ = tx.send(sse_event("response.reasoning_summary.delta", delta));
                    }
                    OutputPart::Call(_) => {
                        // Tool calls not yet implemented for Responses API
                    }
                },
                OutputChunk::Stop(_finish_reason, usage) => {
                    let n_completion = usage
                        .as_ref()
                        .map(|u| u.completion_tokens as usize)
                        .unwrap_or(0);
                    let completed = build_response_completed_event(
                        &respond_id,
                        &model_name,
                        n_prompt_tokens,
                        n_completion,
                    );
                    let _ = tx.send(sse_event("response.completed", completed));
                }
                OutputChunk::Done => {
                    // Already handled by Stop; this is a no-op
                }
                OutputChunk::Error(e) => {
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
        });
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx_tokio)
        .map(|data| Ok::<_, std::convert::Infallible>(Event::default().data(data)));

    Sse::new(stream).into_response()
}
