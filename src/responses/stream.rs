use axum::{
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
};
use futures_util::StreamExt;
use serde_json::json;
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use crate::{
    chat_response::{ChatResponseParser, ChatResponsePart, ChatStreamItem},
    engine::EngineReply,
    helpers::now_unix_s,
    serve::{ERR_CANCELLED, ERR_ENGINE_SHUTDOWN, STREAM_TIMEOUT, oai_error},
};

use super::response_builder::{
    build_content_part_added_event, build_output_item_added_event, build_reasoning_delta_event,
    build_response_completed_event, build_response_created_event, build_text_delta_event,
    extract_reasoning_from_text,
};

fn sse_event(event_type: &str, data: serde_json::Value) -> String {
    let data_str = serde_json::to_string(&data).unwrap_or_default();
    format!("event: {event_type}\ndata: {data_str}\n\n")
}

pub(crate) fn handle_responses_stream(
    rx: crossbeam::channel::Receiver<EngineReply>,
    mut parser: ChatResponseParser,
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

        parser.parse_stream(rx, STREAM_TIMEOUT, |item, _nth| {
            match item {
                ChatStreamItem::Part(part) => match part {
                    ChatResponsePart::Text(t) => {
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
                    ChatResponsePart::Think(r) => {
                        if !reasoning_started {
                            reasoning_started = true;
                        }
                        reasoning_buffer.push_str(&r);
                        let delta = build_reasoning_delta_event(seq, output_index, &r);
                        seq += 1;
                        let _ = tx.send(sse_event("response.reasoning_summary.delta", delta));
                    }
                    ChatResponsePart::Call(_) => {
                        // Tool calls not yet implemented for Responses API
                    }
                },
                ChatStreamItem::Stop(_finish_reason, usage) => {
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
                ChatStreamItem::Done => {
                    // Already handled by Stop; this is a no-op
                }
                ChatStreamItem::Error(e) => {
                    let err = json!({
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

    let stream = UnboundedReceiverStream::new(rx_tokio)
        .map(|data| Ok::<_, std::convert::Infallible>(Event::default().data(data)));

    Sse::new(stream).into_response()
}
