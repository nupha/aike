// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::collections::VecDeque;

use liter_llm::{
    ChatCompletionChunk, FinishReason, FunctionCall, StreamChoice, StreamDelta, StreamFunctionCall,
    StreamToolCall, ToolCall, ToolType, Usage,
};

use crate::{
    class_llama_engine::{ERR_CANCELLED, ERR_ENGINE_SHUTDOWN},
    engine::{EngineReply, StopReason},
    helpers::now_unix_s,
};

#[derive(Debug)]
pub(crate) enum ChatPart {
    Text(String),
    Think(String),
    Call(ToolCall),
}

impl ChatPart {
    pub fn into_chunk_delta(self) -> StreamDelta {
        match self {
            Self::Text(t) => StreamDelta {
                content: Some(t),
                ..Default::default()
            },
            Self::Think(t) => StreamDelta {
                content: Some(t), // should use reasoning_content
                ..Default::default()
            },
            Self::Call(tool_call) => StreamDelta {
                tool_calls: Some(vec![StreamToolCall {
                    index: 0,
                    id: Some(tool_call.id),
                    call_type: Some(tool_call.call_type),
                    function: Some(StreamFunctionCall {
                        name: Some(tool_call.function.name),
                        arguments: Some(tool_call.function.arguments),
                    }),
                }]),
                ..Default::default()
            },
        }
    }

    pub fn into_chunk(self, id: String, model: String, created: u64) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id,
            created,
            model,
            object: "chat.completion.chunk".to_string(),
            choices: vec![StreamChoice {
                delta: self.into_chunk_delta(),
                index: 0,
                finish_reason: None,
            }],
            ..Default::default()
        }
    }
}

// TODO: combine ChatStreamItem with ChatPart
pub(crate) enum ChatStreamItem {
    Chunk(ChatCompletionChunk),
    Done,
    Error(String),
}

pub struct ChatResponseParser {
    state: State,
    out: VecDeque<ChatPart>,
}

#[derive(Debug, PartialEq)]
enum State {
    Text,
    Thinking,
    MaybeCall(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tag {
    Think,
    CloseThink,
    ToolCall,
    CloseToolCall,
}

impl Tag {
    pub const fn as_str(&self) -> &str {
        match self {
            Tag::Think => "<think>",
            Tag::CloseThink => "</think>",
            Tag::ToolCall => "<tool_call>",
            Tag::CloseToolCall => "</tool_call>",
        }
    }
}

impl ChatResponseParser {
    pub fn new() -> Self {
        Self {
            state: State::Text,
            out: VecDeque::new(),
        }
    }

    /// Push a chunk of streaming text into the parser.
    pub fn push_str(&mut self, input: &str) {
        let mut remaining = input;

        while !remaining.is_empty() {
            // Find the earliest occurrence among all special tags
            let mut think_pos: Option<usize> = None;
            let mut close_think_pos: Option<usize> = None;
            let mut tool_call_pos: Option<usize> = None;
            let mut close_tool_call_pos: Option<usize> = None;

            match self.state {
                State::Text => {
                    think_pos = remaining.find(Tag::Think.as_str());
                    tool_call_pos = remaining.find(Tag::ToolCall.as_str());
                }
                State::Thinking => {
                    close_think_pos = remaining.find(Tag::CloseThink.as_str());
                }
                State::MaybeCall(_) => {
                    close_tool_call_pos = remaining.find(Tag::CloseToolCall.as_str());
                }
            }

            // Pick the earliest tag (smallest position)
            let mut found_tag: Option<(usize, Tag)> = None;

            for (p, t) in [
                (think_pos, Tag::Think),
                (close_think_pos, Tag::CloseThink),
                (tool_call_pos, Tag::ToolCall),
                (close_tool_call_pos, Tag::CloseToolCall),
            ] {
                if let Some(p) = p {
                    match found_tag {
                        None => {
                            found_tag = Some((p, t));
                        }
                        Some((p0, _)) if p0 > p => {
                            found_tag = Some((p, t));
                        }
                        _ => {}
                    }
                }
            }

            match (found_tag, &mut self.state) {
                // STATE TRANSITIONS: THINKING
                (Some((pos, Tag::Think)), State::Text) => {
                    self.state = State::Thinking;
                    if pos > 0 {
                        self.out
                            .push_back(ChatPart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + Tag::Think.as_str().len()..];
                }
                (Some((pos, Tag::CloseThink)), State::Thinking) => {
                    self.state = State::Text;
                    if pos > 0 {
                        self.out
                            .push_back(ChatPart::Think(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + Tag::CloseThink.as_str().len()..];
                }

                // STATE TRANSITIONS: CALL
                (Some((pos, Tag::ToolCall)), State::Text) => {
                    if pos > 0 {
                        self.out
                            .push_back(ChatPart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + Tag::ToolCall.as_str().len()..];
                    self.state = State::MaybeCall(String::new());
                }
                (Some((pos, Tag::CloseToolCall)), State::MaybeCall(s)) => {
                    s.push_str(&remaining[..pos]);
                    remaining = &remaining[pos + Tag::CloseToolCall.as_str().len()..];

                    // Note: we use lark grammar to restrict engine generation, in this case
                    // <tool_call> arguments could be a json schema object rather than a string.
                    // while openai compatibile services returns <tool_call> arguments as string.
                    // we must accept both object and string for arguments
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(s) {
                        let obj = match value.as_object() {
                            Some(obj) if obj.len() == 2 => obj,
                            _ => {
                                // must be an object with exactly 'name' and 'arguments' fields
                                self.out.push_back(ChatPart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        let name = match obj.get("name").and_then(|v| v.as_str()) {
                            Some(n) => n.to_string(),
                            None => {
                                // missing 'name' field
                                self.out.push_back(ChatPart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        let arguments = match obj.get("arguments") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(other) => serde_json::to_string(other).unwrap_or_default(),
                            None => {
                                // missing 'arguments' field
                                self.out.push_back(ChatPart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        self.out.push_back(ChatPart::Call(ToolCall {
                            id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                            call_type: ToolType::Function,
                            function: FunctionCall { name, arguments },
                        }));
                    } else {
                        // not a valid json call
                        self.out.push_back(ChatPart::Text(std::mem::take(s)));
                    }

                    self.state = State::Text;
                }

                // STATE AS-IS
                (Some((pos, t)), State::Text) => {
                    let p = pos + t.as_str().len();
                    self.out
                        .push_back(ChatPart::Text(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::Thinking) => {
                    let p = pos + t.as_str().len();
                    self.out
                        .push_back(ChatPart::Think(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::MaybeCall(s)) => {
                    let p = pos + t.as_str().len();
                    s.push_str(&remaining[..p]);
                    remaining = &remaining[p..];
                }

                // NO TAG
                (None, State::Text) => {
                    debug_assert!(!remaining.is_empty());
                    self.out.push_back(ChatPart::Text(remaining.to_string()));
                    remaining = "";
                }
                (None, State::Thinking) => {
                    debug_assert!(!remaining.is_empty());
                    self.out.push_back(ChatPart::Think(remaining.to_string()));
                    remaining = "";
                }
                (None, State::MaybeCall(s)) => {
                    debug_assert!(!remaining.is_empty());
                    s.push_str(remaining);
                    remaining = "";
                }
            }
        }
    }

    pub fn end(&mut self) {
        if let State::MaybeCall(s) = std::mem::replace(&mut self.state, State::Text) {
            self.out
                .push_back(ChatPart::Text(format!("{}{s}", Tag::ToolCall.as_str())));
        }
    }

    pub fn output(&self) -> &VecDeque<ChatPart> {
        &self.out
    }

    pub fn next(&mut self) -> Option<ChatPart> {
        self.out.pop_front()
    }

    pub fn state(&self) -> &str {
        match self.state {
            State::Text => "Text",
            State::Thinking => "Thinking",
            State::MaybeCall(_) => "MaybeCall",
        }
    }
}

/// Reading messages from Receiver<EngineReply>, parse and call `on_item` on each parsed elements.
/// If `timeout` elapses without a message, emits an error and stops.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_response_listener<F>(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    include_usage: bool,
    n_prompt_tokens: usize,
    mut parser: ChatResponseParser,
    mut on_item: F,
) where
    F: FnMut(ChatStreamItem) -> bool,
{
    let respond_id = respond_id.to_string();
    let model_name = model_name.to_string();
    let mut sent_started = false;

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Text(t)) => {
                let created = now_unix_s() as u64;

                if !sent_started {
                    // Send first chunk to identify role
                    sent_started = true;
                    let first = ChatCompletionChunk {
                        id: respond_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model_name.clone(),
                        choices: vec![StreamChoice {
                            delta: StreamDelta {
                                role: Some("assistant".to_string()),
                                ..Default::default()
                            },
                            ..Default::default()
                        }],
                        ..Default::default()
                    };
                    if !on_item(ChatStreamItem::Chunk(first)) {
                        break;
                    }
                }

                // parse response into parts
                parser.push_str(&t);

                while let Some(part) = parser.next() {
                    let chunk = part.into_chunk(respond_id.clone(), model_name.clone(), created);
                    if !on_item(ChatStreamItem::Chunk(chunk)) {
                        break;
                    }
                }
            }
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                let created = now_unix_s() as u64;

                // to flush unfinished <tool_call> as normal text
                parser.end();
                while let Some(el) = parser.next() {
                    let chunk = el.into_chunk(respond_id.clone(), model_name.clone(), created);
                    if !on_item(ChatStreamItem::Chunk(chunk)) {
                        break;
                    }
                }

                // the last chunk indicates finish reason and optionally usage
                let last = ChatCompletionChunk {
                    id: respond_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created,
                    model: model_name.clone(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: StreamDelta::default(),
                        finish_reason: Some(match reason {
                            StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                                FinishReason::Stop
                            }
                            StopReason::MaxTokens => FinishReason::Length,
                        }),
                    }],
                    usage: if include_usage {
                        Some(Usage {
                            prompt_tokens: n_prompt_tokens as u64,
                            completion_tokens: n_completion_tokens as u64,
                            total_tokens: (n_prompt_tokens + n_completion_tokens) as u64,
                        })
                    } else {
                        None
                    },
                    ..Default::default()
                };
                on_item(ChatStreamItem::Chunk(last));
                on_item(ChatStreamItem::Done);
                break;
            }
            Ok(EngineReply::Cancelled) => {
                on_item(ChatStreamItem::Error(ERR_CANCELLED.to_string()));
                break;
            }
            Ok(EngineReply::Error(e)) => {
                on_item(ChatStreamItem::Error(e));
                break;
            }
            Ok(EngineReply::Shutdown) => {
                on_item(ChatStreamItem::Error(ERR_ENGINE_SHUTDOWN.to_string()));
                break;
            }
            Err(_) => {
                on_item(ChatStreamItem::Error("stream timed out".to_string()));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_output(p: &ChatResponseParser) -> String {
        p.output()
            .iter()
            .filter_map(|e| match e {
                ChatPart::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reasoning_output(p: &ChatResponseParser) -> String {
        p.output()
            .iter()
            .filter_map(|e| match e {
                ChatPart::Think(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_passthrough() {
        let mut p = ChatResponseParser::new();
        p.push_str("Hello, world!");
        assert_eq!(text_output(&p), "Hello, world!");
    }

    #[test]
    fn think_tag_basic() {
        let mut p = ChatResponseParser::new();
        p.push_str("Hello <think>hmm</think> world");
        assert_eq!(text_output(&p), "Hello  world");
        assert_eq!(reasoning_output(&p), "hmm");
        assert_eq!(p.state(), "Text");
    }

    #[test]
    fn think_tag_multiple_chunks() {
        let mut p = ChatResponseParser::new();
        p.push_str("Hello <think>");
        assert_eq!(p.state(), "Thinking");
        assert_eq!(text_output(&p), "Hello ");
        p.push_str("let me think");
        p.push_str(" carefully");
        p.push_str("</think> done");
        assert_eq!(p.state(), "Text");
        assert_eq!(reasoning_output(&p), "let me think carefully");
        assert_eq!(text_output(&p), "Hello  done");
    }

    #[test]
    fn tool_call_valid_json() {
        let mut p = ChatResponseParser::new();
        // No text before <tool_call> — the MaybeCall buffer is empty,
        // so the JSON parses correctly as FunctionCall
        p.push_str(r#"<tool_call>{"name":"get_weather","arguments":"{}"}</tool_call>"#);
        assert_eq!(p.state(), "Text");
        assert_eq!(text_output(&p), "");
        assert_eq!(p.output().len(), 1);
        match &p.output()[0] {
            ChatPart::Call(fc) => {
                assert_eq!(fc.function.name, "get_weather");
                assert_eq!(fc.function.arguments, "{}");
            }
            _ => panic!("expected Call element"),
        }
    }

    #[test]
    fn tool_call_invalid_json_fallback() {
        let mut p = ChatResponseParser::new();
        p.push_str("bad: <tool_call>{invalid}</tool_call>");
        assert_eq!(p.state(), "Text");
        // Invalid JSON: tags are stripped, text before + content becomes Text
        assert_eq!(text_output(&p), "bad: {invalid}");
    }

    #[test]
    fn tool_call_partial_then_invalid() {
        let mut p = ChatResponseParser::new();
        p.push_str("start <tool_call>{\"key\"");
        assert_eq!(p.state(), "MaybeCall");
        assert_eq!(text_output(&p), "");
        // No </tool_call> tag, so parser stays in MaybeCall
        p.push_str(": invalid}");
        assert_eq!(p.state(), "MaybeCall");
    }

    #[test]
    fn tool_call_multiple_chunks_valid() {
        let mut p = ChatResponseParser::new();
        // No text before <tool_call>
        p.push_str(r#"<tool_call>{"name":"#);
        assert_eq!(p.state(), "MaybeCall");
        assert_eq!(text_output(&p), "");
        p.push_str(r#""search""#);
        p.push_str(r#","arguments":"{}"}</tool_call> b"#);
        assert_eq!(p.state(), "Text");
        // Text after </tool_call> is emitted as Text
        assert_eq!(text_output(&p), " b");
        assert_eq!(p.output().len(), 2);
        match &p.output()[0] {
            ChatPart::Call(fc) => {
                assert_eq!(fc.function.name, "search");
                assert_eq!(fc.function.arguments, "{}");
            }
            _ => panic!("expected Call element"),
        }
    }

    #[test]
    fn mixed_think_and_tool_call() {
        let mut p = ChatResponseParser::new();
        p.push_str(
            r#"<think>reasoning</think> <tool_call>{"name":"calc","arguments":"{}"}</tool_call>"#,
        );
        assert_eq!(reasoning_output(&p), "reasoning");
        // Text before <tool_call> (" ") is buffered in MaybeCall and prepended
        // to the JSON content, but serde_json ignores leading whitespace,
        // so the JSON still parses as a valid FunctionCall
        assert_eq!(text_output(&p), "");
        assert_eq!(p.output().len(), 3);
        match &p.output()[0] {
            ChatPart::Text(s) => assert_eq!(s, ""),
            _ => panic!("expected Text element"),
        }
        match &p.output()[1] {
            ChatPart::Think(s) => assert_eq!(s, "reasoning"),
            _ => panic!("expected Reasoning element"),
        }
        match &p.output()[2] {
            ChatPart::Call(fc) => {
                assert_eq!(fc.function.name, "calc");
                assert_eq!(fc.function.arguments, "{}");
            }
            _ => panic!("expected Call element"),
        }
    }

    #[test]
    fn no_tags() {
        let mut p = ChatResponseParser::new();
        p.push_str("just text");
        assert_eq!(text_output(&p), "just text");
    }

    #[test]
    fn empty_input() {
        let mut p = ChatResponseParser::new();
        p.push_str("");
        assert!(p.output().is_empty());
    }

    #[test]
    fn orphan_close_think_in_text() {
        let mut p = ChatResponseParser::new();
        p.push_str("hello </think> world");
        assert_eq!(text_output(&p), "hello </think> world");
    }

    #[test]
    fn orphan_close_tool_call_in_text() {
        let mut p = ChatResponseParser::new();
        p.push_str("hello </tool_call> world");
        assert_eq!(text_output(&p), "hello </tool_call> world");
    }

    #[test]
    fn tool_call_nested_think_inside() {
        let mut p = ChatResponseParser::new();
        // No text before <tool_call>
        p.push_str(r#"<tool_call>{"name":"<think>","arguments":"{}"}</tool_call>"#);
        assert_eq!(p.output().len(), 1);
        match &p.output()[0] {
            ChatPart::Call(fc) => {
                assert_eq!(fc.function.name, "<think>");
            }
            _ => panic!("expected Call element"),
        }
    }
}
