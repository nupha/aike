use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, InputItem, InputParam, MessageType, Role,
};
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::{
    chat_response::ChatResponseParser,
    engine::{Engine, GrammarConfig, OutputMode, TaskConfig},
    helpers::{build_chat_prompt_tokens, compile_tools_grammar},
    serve::{
        AppState, DEFAULT_TEMPERATURE, DEFAULT_TOP_P, ERR_CANCELLED, ERR_ENGINE_SHUTDOWN,
        STREAM_TIMEOUT, oai_error,
    },
};

use super::convert::{input_item_to_chat_message, responses_tools_to_chat_tools};
use super::response_builder::{build_responses_response, extract_reasoning_from_text};
use super::stream::handle_responses_stream;

pub async fn handle_responses(
    State(state): State<AppState>,
    Json(req): Json<CreateResponse>,
) -> Response {
    let is_stream = req.stream.unwrap_or(false);

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

    let chat_tools = req.tools.as_ref().map(|t| responses_tools_to_chat_tools(t));
    let tool_choice = None;

    let has_reasoning = req.reasoning.is_some();

    let n_prompt_tokens;
    let respond_id;
    let model_name;
    let rx;
    let max_tokens;
    let temperature;
    let top_p;

    // Lock engine in a block so MutexGuard is dropped before any await
    {
        let mut engine = state.engine.lock().unwrap();

        max_tokens = req
            .max_output_tokens
            .unwrap_or(engine.default_max_tokens() as u32) as usize;
        temperature = req.temperature.unwrap_or(DEFAULT_TEMPERATURE);
        top_p = req.top_p.unwrap_or(DEFAULT_TOP_P);

        let prompt_tokens = match build_chat_prompt_tokens(
            engine.model(),
            &messages,
            chat_tools.as_deref(),
            tool_choice.as_ref(),
            None,
            req.reasoning.as_ref().and_then(|r| r.effort.as_ref()),
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

        if let Some(tools) = chat_tools.as_ref().filter(|t| !t.is_empty()) {
            let tc = llama_cpp_ffi::ToolChoice::Auto;
            task_config = task_config.grammar(GrammarConfig {
                lark_grammar: compile_tools_grammar(tools, tc),
                tool_choice: tc,
            });
        }

        n_prompt_tokens = prompt_tokens.len();
        respond_id = format!("resp-{}", Uuid::new_v4().simple());
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

    let mut parser = ChatResponseParser::new(has_reasoning, n_prompt_tokens, true);
    parser.set_respond_id(respond_id.clone());
    parser.set_model_name(model_name.clone());

    if is_stream {
        handle_responses_stream(rx, parser, respond_id, model_name)
    } else {
        handle_responses_non_stream(rx, parser, respond_id, model_name).await
    }
}

async fn handle_responses_non_stream(
    rx: crossbeam::channel::Receiver<crate::engine::EngineReply>,
    parser: ChatResponseParser,
    respond_id: String,
    model_name: String,
) -> Response {
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
        Ok(Ok(val)) => {
            let text = val["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let usage = &val["usage"];
            let n_prompt = usage["prompt_tokens"].as_u64().unwrap_or(0) as usize;
            let n_completion = usage["completion_tokens"].as_u64().unwrap_or(0) as usize;

            let (reasoning, clean_text) = extract_reasoning_from_text(&text);

            let resp = build_responses_response(
                &respond_id,
                &model_name,
                &clean_text,
                reasoning.as_deref(),
                n_prompt,
                n_completion,
            );
            Json(resp).into_response()
        }
        Ok(Err((code, err_info))) => (code, Json(err_info)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(oai_error("request task failed", "server_error")),
        )
            .into_response(),
    }
}
