use serde_json::json;
use uuid::Uuid;

use crate::helpers::now_unix_s;

/// Extract reasoning text from <think> tags and return clean text.
pub fn extract_reasoning_from_text(text: &str) -> (Option<String>, String) {
    let think_start = "<think>";
    let think_end = "</think>";

    if let Some(start) = text.find(think_start) {
        let after_start = &text[start + think_start.len()..];
        if let Some(end) = after_start.find(think_end) {
            let reasoning = after_start[..end].to_string();
            let clean = format!(
                "{}{}",
                &text[..start],
                &after_start[end + think_end.len()..]
            );
            return (Some(reasoning), clean.trim().to_string());
        }
    }
    (None, text.to_string())
}

/// Build a non-streaming Responses API response body.
pub fn build_responses_response(
    respond_id: &str,
    model_name: &str,
    text: &str,
    reasoning_text: Option<&str>,
    n_prompt_tokens: usize,
    n_completion_tokens: usize,
) -> serde_json::Value {
    let now = now_unix_s() as u64;

    let mut output_items: Vec<serde_json::Value> = Vec::new();

    if let Some(reasoning) = reasoning_text {
        if !reasoning.is_empty() {
            output_items.push(json!({
                "type": "reasoning",
                "id": format!("rsn_{}", Uuid::new_v4().simple()),
                "summary": [
                    {
                        "type": "summary_text",
                        "text": reasoning
                    }
                ]
            }));
        }
    }

    output_items.push(json!({
        "type": "message",
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "role": "assistant",
        "status": "completed",
        "content": [
            {
                "type": "output_text",
                "text": text,
                "annotations": []
            }
        ]
    }));

    json!({
        "id": respond_id,
        "object": "response",
        "created_at": now,
        "completed_at": now,
        "status": "completed",
        "model": model_name,
        "output": output_items,
        "usage": {
            "input_tokens": n_prompt_tokens,
            "output_tokens": n_completion_tokens,
            "total_tokens": n_prompt_tokens + n_completion_tokens,
            "input_tokens_details": {
                "cached_tokens": 0
            },
            "output_tokens_details": {
                "reasoning_tokens": 0
            }
        }
    })
}

/// Build a "response.created" event for SSE stream.
pub fn build_response_created_event(respond_id: &str, model_name: &str) -> serde_json::Value {
    json!({
        "id": respond_id,
        "object": "response",
        "created_at": now_unix_s() as u64,
        "status": "in_progress",
        "model": model_name,
    })
}

/// Build a "response.output_item.added" event for a message item.
pub fn build_output_item_added_event(seq: u64, output_index: u32) -> serde_json::Value {
    json!({
        "type": "response.output_item.added",
        "sequence_number": seq,
        "output_index": output_index,
        "item": {
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "role": "assistant",
            "status": "in_progress",
            "content": []
        }
    })
}

/// Build a "response.content_part.added" event for a text part.
pub fn build_content_part_added_event(
    seq: u64,
    output_index: u32,
    content_index: u32,
) -> serde_json::Value {
    json!({
        "type": "response.content_part.added",
        "sequence_number": seq,
        "output_index": output_index,
        "content_index": content_index,
        "part": {
            "type": "output_text",
            "text": "",
            "annotations": []
        }
    })
}

/// Build a "response.output_text.delta" event.
pub fn build_text_delta_event(
    seq: u64,
    output_index: u32,
    content_index: u32,
    delta: &str,
) -> serde_json::Value {
    json!({
        "type": "response.output_text.delta",
        "sequence_number": seq,
        "output_index": output_index,
        "content_index": content_index,
        "delta": delta
    })
}

/// Build a "response.reasoning_summary.delta" event.
pub fn build_reasoning_delta_event(seq: u64, output_index: u32, delta: &str) -> serde_json::Value {
    json!({
        "type": "response.reasoning_summary.delta",
        "sequence_number": seq,
        "output_index": output_index,
        "delta": delta
    })
}

/// Build a "response.completed" event.
pub fn build_response_completed_event(
    respond_id: &str,
    model_name: &str,
    n_prompt_tokens: usize,
    n_completion_tokens: usize,
) -> serde_json::Value {
    json!({
        "id": respond_id,
        "object": "response",
        "completed_at": now_unix_s() as u64,
        "status": "completed",
        "model": model_name,
        "usage": {
            "input_tokens": n_prompt_tokens,
            "output_tokens": n_completion_tokens,
            "total_tokens": n_prompt_tokens + n_completion_tokens,
            "input_tokens_details": {
                "cached_tokens": 0
            },
            "output_tokens_details": {
                "reasoning_tokens": 0
            }
        }
    })
}
