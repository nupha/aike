use serde_json::json;

use crate::output::{OutputPart, OutputChunk};

use super::response_builder::build_response_completed_event;

/// Convert a ChatStreamItem to a ResponseStreamEvent-shaped JSON value.
/// Used by Engine.responses() to produce the same event format as the SSE stream.
pub(crate) fn chat_stream_item_to_response_event(
    item: &OutputChunk,
    respond_id: &str,
    model_name: &str,
    nth: usize,
) -> serde_json::Value {
    match item {
        OutputChunk::Part(part) => match part {
            OutputPart::Text(t) => {
                if nth == 0 {
                    // First text chunk: emit output_item.added + content_part.added + text.delta
                    json!({
                        "type": "response.output_text.delta",
                        "delta": t,
                        "output_index": 0,
                        "content_index": 0
                    })
                } else {
                    json!({
                        "type": "response.output_text.delta",
                        "delta": t,
                        "output_index": 0,
                        "content_index": 0
                    })
                }
            }
            OutputPart::Reasoning(r) => {
                json!({
                    "type": "response.reasoning_summary.delta",
                    "delta": r,
                    "output_index": 0
                })
            }
            OutputPart::Call(_) => {
                json!({
                    "type": "response.output_text.delta",
                    "delta": "",
                    "output_index": 0,
                    "content_index": 0
                })
            }
        },
        OutputChunk::Stop(_finish_reason, usage) => {
            let n_completion = usage
                .as_ref()
                .map(|u| u.completion_tokens as usize)
                .unwrap_or(0);
            build_response_completed_event(respond_id, model_name, 0, n_completion)
        }
        OutputChunk::Done => {
            json!({"type": "done"})
        }
        OutputChunk::Error(e) => {
            json!({
                "type": "error",
                "error": {
                    "message": e,
                    "type": "server_error"
                }
            })
        }
    }
}

pub(crate) fn sse_event(event_type: &str, data: serde_json::Value) -> String {
    let data_str = serde_json::to_string(&data).unwrap_or_default();
    format!("event: {event_type}\ndata: {data_str}\n\n")
}
