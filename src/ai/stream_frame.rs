// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Inbound SSE frame handling for chat-completions streams.
//!
//! One frame's whole journey lives here: parse the `data:` payload (salvaging
//! instead of silently dropping), normalize the message shape, accumulate
//! `tool_calls` deltas across frames, and coalesce pure-content deltas. The
//! stream loop in `class_client.rs` consumes these types; nothing here knows
//! about HTTP, cancellation or the JS boundary.

use super::{OUTMSG_FIELD_CHOICES, OUTMSG_FIELD_REASONING};

/// Normalize message format in `choices[]` to standard shape.
/// For both streaming delta and non-streaming message.
pub(super) fn normalize_message_shape(obj: &mut serde_json::Value) {
    if let Some(choices) = obj[OUTMSG_FIELD_CHOICES].as_array_mut() {
        for choice in choices.iter_mut() {
            // The target is either "delta" (streaming) or "message" (non-streaming).
            let target = match choice.as_object_mut() {
                Some(m) => {
                    if let Some(v) = m.get_mut("delta") {
                        v.as_object_mut()
                    } else if let Some(v) = m.get_mut("message") {
                        v.as_object_mut()
                    } else {
                        None
                    }
                }
                None => None,
            };

            if let Some(target) = target {
                // 1. fix reasoning -> reasoning_content
                if let Some(val) = target.remove("reasoning")
                    && !target.contains_key(OUTMSG_FIELD_REASONING)
                {
                    target.insert(OUTMSG_FIELD_REASONING.to_string(), val);
                }
            }
        }
    }
}

/// Escape bare control characters inside the string literals of a JSON text.
///
/// Returns `Some(repaired)` when at least one escape was added, `None` when
/// nothing needed changing (or the text isn't worth attempting).
fn escape_control_chars(text: &str) -> Option<String> {
    // Track string context so only in-string control chars are escaped:
    // `\"` toggles state, `\\` escapes the next char (a control char after a
    // backslash is part of an escape sequence, not a bare one). Whitespace
    // outside strings is legal JSON and must be left alone.
    let mut out = String::with_capacity(text.len() + 8);
    let mut in_string = false;
    let mut escaped = false;
    let mut salvaged = false;
    for c in text.chars() {
        if escaped {
            escaped = false;
        } else if in_string && c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_string = !in_string;
        } else if in_string && (c as u32) < 0x20 {
            salvaged = true;
            out.push_str(&format!("\\u{:04x}", c as u32));
            continue;
        }
        out.push(c);
    }
    salvaged.then_some(out)
}

/// Parse one SSE `data:` payload, salvaging bare control characters instead of
/// dropping the frame.
///
/// RFC 8259 requires control characters (U+0000–U+001F) inside a string to be
/// escaped (`\u0007`); a gateway that forwards model text verbatim can put a
/// raw byte there, and serde_json then rejects the WHOLE frame. Silently
/// discarding it truncates the tool-call `arguments` stream mid-payload — a
/// malformed fragment is stored, every later request replays it, and the
/// provider 400s the session. When the frame is broken beyond repair the
/// caller logs it — a payload never just disappears.
pub(super) fn parse_sse_payload(data: &str) -> Option<serde_json::Value> {
    let mut json = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(v) => v,
        Err(e) => {
            let Some(repaired) = escape_control_chars(data) else {
                log::warn!(
                    "SSE frame dropped (unparseable JSON, {e}): data={}",
                    data.chars().take(200).collect::<String>()
                );
                return None;
            };
            match serde_json::from_str::<serde_json::Value>(&repaired) {
                Ok(v) => {
                    log::warn!(
                        "SSE frame needed control-char salvage: data={}",
                        data.chars().take(200).collect::<String>()
                    );
                    v
                }
                Err(e) => {
                    log::warn!(
                        "SSE frame unparseable after salvage ({e}): data={}",
                        data.chars().take(200).collect::<String>()
                    );
                    return None;
                }
            }
        }
    };

    repair_tool_call_arguments(&mut json);
    Some(json)
}

/// `function.arguments` is a string whose VALUE is JSON text. The outer frame
/// can parse while that inner text still carries a bare control char the
/// model emitted — `JSON.parse` downstream then rejects the call, the
/// truncated-looking payload enters history and poisons every later request.
/// The string context is known here, so escape it at the source.
fn repair_tool_call_arguments(frame: &mut serde_json::Value) {
    // Indexing a missing key yields Null and `[0]` on Null PANICS — walk with
    // get() so a frame without choices/tool_calls is a no-op, not a crash.
    let Some(tcs) = frame
        .get_mut(OUTMSG_FIELD_CHOICES)
        .and_then(|c| c.get_mut(0))
        .and_then(|c| c.get_mut("delta"))
        .and_then(|d| d.get_mut("tool_calls"))
        .and_then(|t| t.as_array_mut())
    else {
        return;
    };
    for tc in tcs {
        let Some(arg_val) = tc.get_mut("function").and_then(|f| f.get_mut("arguments")) else {
            continue;
        };
        let Some(args) = arg_val.as_str() else {
            continue;
        };
        if let Some(repaired) = escape_control_chars(args) {
            log::warn!(
                "tool_call arguments needed control-char repair: {}",
                args.chars().take(200).collect::<String>()
            );
            *arg_val = serde_json::Value::String(repaired);
        }
    }
}

/// Accumulates tool_call deltas until complete, then emits a single chunk.
/// Fields are `pub(super)` because the stream loop's flush log names them.
pub(super) struct ToolCallBuffer {
    pub(super) idx: usize,
    pub(super) id: Option<String>,
    pub(super) r#type: Option<String>,
    pub(super) name: Option<String>,
    pub(super) arguments: String,
    // Original chunk metadata preserved for completeness
    chunk_id: Option<String>,
    chunk_object: Option<String>,
    chunk_created: Option<i64>,
    chunk_model: Option<String>,
}

impl ToolCallBuffer {
    pub(super) fn new(idx: usize) -> Self {
        Self {
            idx,
            id: None,
            r#type: None,
            name: None,
            arguments: String::new(),
            chunk_id: None,
            chunk_object: None,
            chunk_created: None,
            chunk_model: None,
        }
    }

    pub(super) fn apply_delta(&mut self, tc: &serde_json::Value, original: &serde_json::Value) {
        if let Some(id) = tc["id"].as_str() {
            self.id = Some(id.to_owned());
        }
        if let Some(t) = tc["type"].as_str() {
            self.r#type = Some(t.to_owned());
        }
        if let Some(name) = tc["function"]["name"].as_str() {
            self.name = Some(name.to_owned());
        }
        if let Some(args) = tc["function"]["arguments"].as_str() {
            self.arguments.push_str(args);
        }
        // Capture chunk-level metadata from the first chunk that triggers tool_call accumulation
        if self.chunk_id.is_none() {
            self.chunk_id = original["id"].as_str().map(String::from);
            self.chunk_object = original["object"].as_str().map(String::from);
            self.chunk_created = original["created"].as_i64();
            self.chunk_model = original["model"].as_str().map(String::from);
        }
    }

    /// A tool_call is complete only once the stream has supplied its `id` and
    /// `function.name`. Until then it is a partial fragment that must not be
    /// surfaced or stored.
    pub(super) fn is_complete(&self) -> bool {
        self.id.is_some() && self.name.is_some()
    }

    pub(super) fn into_chunk(self) -> serde_json::Value {
        serde_json::json!({
            "id": self.chunk_id,
            "object": self.chunk_object,
            "created": self.chunk_created,
            "model": self.chunk_model,
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": self.idx,
                        "id": self.id,
                        "type": self.r#type,
                        "function": {
                            "name": self.name,
                            "arguments": self.arguments,
                        }
                    }]
                },
                "finish_reason": null
            }]
        })
    }
}

/// What one finished piece of stream output is. Sent from the reading task to
/// the JS-side iterator through a promise slot.
pub(super) enum ChatReply {
    Chunk(serde_json::Value), // Use raw JSON to preserve extension fields
    Done,
    Error(String),
}

/// Accumulates consecutive pure-content deltas into a single chunk. Active only
/// with the `coalesce` call opt: a fast (char-per-chunk) stream then amortizes
/// the per-chunk commit/wake/JS-convert cost over several deltas.
pub(super) struct CoalescedChunk {
    id: Option<String>,
    object: Option<String>,
    created: Option<i64>,
    model: Option<String>,
    content: String,
    reasoning: String,
}

impl CoalescedChunk {
    pub(super) fn new(json: &serde_json::Value) -> Self {
        Self {
            id: json["id"].as_str().map(String::from),
            object: json["object"].as_str().map(String::from),
            created: json["created"].as_i64(),
            model: json["model"].as_str().map(String::from),
            content: String::new(),
            reasoning: String::new(),
        }
    }

    pub(super) fn apply_delta(&mut self, json: &serde_json::Value) {
        let delta = &json[OUTMSG_FIELD_CHOICES][0]["delta"];
        if let Some(c) = delta["content"].as_str() {
            self.content.push_str(c);
        }
        // Some servers emit "reasoning", others "reasoning_content".
        if let Some(r) = delta[OUTMSG_FIELD_REASONING].as_str() {
            self.reasoning.push_str(r);
        } else if let Some(r) = delta["reasoning"].as_str() {
            self.reasoning.push_str(r);
        }
    }

    pub(super) fn into_chunk(self) -> serde_json::Value {
        let mut delta = serde_json::Map::new();
        if !self.content.is_empty() {
            delta.insert("content".into(), serde_json::Value::String(self.content));
        }
        if !self.reasoning.is_empty() {
            delta.insert(
                OUTMSG_FIELD_REASONING.into(),
                serde_json::Value::String(self.reasoning),
            );
        }
        serde_json::json!({
            "id": self.id,
            "object": self.object,
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": null
            }]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parse_sse_payload;

    #[test]
    fn parses_well_formed_frame() {
        let v = parse_sse_payload(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
        assert_eq!(v["choices"][0]["delta"]["content"], "hi");
    }

    // The incident shape: a raw BEL byte inside the `arguments` string.
    // Two layers to repair: the frame itself won't parse with a bare control
    // char in it, and once the frame parses the inner `arguments` JSON text
    // still carries that char. Both must survive — the frame is never
    // dropped, and the inner text must become parseable downstream.
    #[test]
    fn salvages_bare_control_char_in_string() {
        let frame = "{\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":".to_string()
            + "{\"arguments\":\"{\\\"new_text\\\": \\\"echo hi\u{7}\\\"}\"}}]}}]}";
        let v = parse_sse_payload(&frame).expect("bare control char must be salvaged");
        let args = v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        // Inner text is now valid JSON (escaped, not raw) …
        let inner: serde_json::Value =
            serde_json::from_str(args).expect("inner arguments must parse after repair");
        // … and the original char is preserved in the value.
        assert!(
            inner["new_text"].as_str().unwrap().contains('\u{7}'),
            "repair keeps the original char: {inner:?}"
        );
    }

    // Outer frame syntax is clean: the control char is a legal escape in the
    // OUTER json, but decodes to a bare byte inside the arguments VALUE.
    #[test]
    fn repairs_inner_arguments_when_frame_is_clean() {
        let frame = "{\"choices\":[{\"delta\":{\"content\":\"ok\",\"tool_calls\":[{\"function\":"
            .to_string()
            + "{\"name\":\"t\",\"arguments\":\"{\\\"s\\\": \\\"a\\u0001b\\\"}\"}}]}}]}";
        let v = parse_sse_payload(&frame).expect("frame with clean syntax parses as-is");
        let args = v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let inner: serde_json::Value = serde_json::from_str(args).expect("inner repaired");
        assert_eq!(inner["s"].as_str().unwrap(), "a\u{1}b");
    }

    #[test]
    fn leaves_whitespace_outside_strings_alone() {
        assert!(parse_sse_payload("{ \"a\" : 1 }").is_some());
    }

    #[test]
    fn returns_none_when_beyond_salvage() {
        // Truncated JSON with no in-string control char: nothing to salvage.
        assert!(parse_sse_payload("{\"a\": ").is_none());
        // Broken even after escaping: the string never closes.
        assert!(parse_sse_payload("{\"a\": \"unclosed\u{7}").is_none());
    }
}
