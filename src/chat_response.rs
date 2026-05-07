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
pub(crate) enum ChatElement {
    Text(String),
    Reasoning(String),
    Call(ToolCall),
}

// TODO: combine ChatStreamItem with ChatElement
pub(crate) enum ChatStreamItem {
    Chunk(ChatCompletionChunk),
    Done,
    Error(String),
}

pub struct ChatResponseParser {
    state: State,
    out: VecDeque<ChatElement>,
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
            let think_pos = remaining.find(Tag::Think.as_str());
            let close_think_pos = remaining.find(Tag::CloseThink.as_str());
            let tool_call_pos = remaining.find(Tag::ToolCall.as_str());
            let close_tool_call_pos = remaining.find(Tag::CloseToolCall.as_str());

            // Pick the earliest tag (smallest position)
            let mut found_tag: Option<(usize, Tag)> = None;
            if let Some(p) = think_pos {
                found_tag = Some((p, Tag::Think));
            }
            if let Some(p) = close_think_pos
                && found_tag.is_none_or(|(e, _)| p < e)
            {
                found_tag = Some((p, Tag::CloseThink));
            }
            if let Some(p) = tool_call_pos
                && found_tag.is_none_or(|(e, _)| p < e)
            {
                found_tag = Some((p, Tag::ToolCall));
            }
            if let Some(p) = close_tool_call_pos
                && found_tag.is_none_or(|(e, _)| p < e)
            {
                found_tag = Some((p, Tag::CloseToolCall));
            }

            match (found_tag, &mut self.state) {
                // STATE TRANSITIONS
                (Some((pos, Tag::Think)), State::Text) => {
                    self.state = State::Thinking;
                    self.out
                        .push_back(ChatElement::Text(remaining[..pos].to_string()));
                    remaining = &remaining[pos + Tag::Think.as_str().len()..];
                }
                (Some((pos, Tag::ToolCall)), State::Text) => {
                    self.state = State::MaybeCall(remaining[..pos].to_string());
                    remaining = &remaining[pos + Tag::ToolCall.as_str().len()..];
                }
                (Some((pos, Tag::CloseThink)), State::Thinking) => {
                    self.state = State::Text;
                    self.out
                        .push_back(ChatElement::Reasoning(remaining[..pos].to_string()));
                    remaining = &remaining[pos + "</think>".len()..];
                }
                (Some((pos, Tag::CloseToolCall)), State::MaybeCall(s)) => {
                    s.push_str(&remaining[..pos]);
                    remaining = &remaining[pos + "</tool_call>".len()..];

                    if let Ok(fc) = serde_json::from_str::<FunctionCall>(s) {
                        self.out.push_back(ChatElement::Call(ToolCall {
                            id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                            call_type: ToolType::Function,
                            function: fc,
                        }));
                    } else {
                        // not a valid function call
                        self.out.push_back(ChatElement::Text(std::mem::take(s)));
                    }

                    self.state = State::Text;
                }

                // STATE AS-IS
                (Some((pos, t)), State::Text) => {
                    let p = pos + t.as_str().len();
                    self.out
                        .push_back(ChatElement::Text(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::Thinking) => {
                    let p = pos + t.as_str().len();
                    self.out
                        .push_back(ChatElement::Reasoning(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::MaybeCall(s)) => {
                    let p = pos + t.as_str().len();
                    s.push_str(&remaining[..p]);
                    remaining = &remaining[p..];
                }

                // NO TAG
                (None, State::Text) => {
                    self.out.push_back(ChatElement::Text(remaining.to_string()));
                    remaining = "";
                }
                (None, State::Thinking) => {
                    self.out
                        .push_back(ChatElement::Reasoning(remaining.to_string()));
                    remaining = "";
                }
                (None, State::MaybeCall(s)) => {
                    s.push_str(remaining);
                    remaining = "";
                }
            }
        }
    }

    pub fn end(&mut self) {
        if let State::MaybeCall(s) = std::mem::replace(&mut self.state, State::Text) {
            self.out
                .push_back(ChatElement::Text(format!("{}{s}", Tag::ToolCall.as_str())));
        }
    }

    pub fn output(&self) -> &VecDeque<ChatElement> {
        &self.out
    }

    pub fn next(&mut self) -> Option<ChatElement> {
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

/// Parse message recevied from Receiver<EngineReply> and call on_item on parsed elements
/// If `timeout` elapses without a message, emits an error and stops.
pub(crate) fn chat_reply_listener<F>(
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
                    if !on_item(ChatStreamItem::Chunk(first)) {
                        break;
                    }
                }

                // parse response
                parser.push_str(&t);

                while let Some(el) = parser.next() {
                    let delta = match el {
                        ChatElement::Text(t) => StreamDelta {
                            content: Some(t),
                            ..Default::default()
                        },
                        ChatElement::Reasoning(t) => StreamDelta {
                            content: Some(t), // should use reasoning_content
                            ..Default::default()
                        },
                        ChatElement::Call(tool_call) => StreamDelta {
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
                    };

                    let chunk = ChatCompletionChunk {
                        id: respond_id.clone(),
                        object: "chat.completion.chunk".to_string(),
                        created,
                        model: model_name.clone(),
                        choices: vec![StreamChoice {
                            index: 0,
                            delta,
                            finish_reason: None,
                        }],
                        ..Default::default()
                    };
                    if !on_item(ChatStreamItem::Chunk(chunk)) {
                        break;
                    }
                }

                // let chunk = ChatCompletionChunk {
                //     id: respond_id.clone(),
                //     object: "chat.completion.chunk".to_string(),
                //     created,
                //     model: model_name.clone(),
                //     choices: vec![StreamChoice {
                //         index: 0,
                //         delta: StreamDelta {
                //             role: None,
                //             content: Some(t),
                //             ..Default::default()
                //         },
                //         finish_reason: None,
                //     }],
                //     ..Default::default()
                // };
                // if !on_item(ChatStreamItem::Chunk(chunk)) {
                //     break;
                // }
            }
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                // to flush unfinished <tool_call> as normal text
                parser.end();
                if let Some(el) = parser.next() {
                    if let ChatElement::Text(s) = el {
                        let delta = StreamDelta {
                            content: Some(s),
                            ..Default::default()
                        };
                        let chunk = ChatCompletionChunk {
                            id: respond_id.clone(),
                            object: "chat.completion.chunk".to_string(),
                            created: now_unix_s() as u64,
                            model: model_name.clone(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta,
                                finish_reason: None,
                            }],
                            ..Default::default()
                        };
                        if !on_item(ChatStreamItem::Chunk(chunk)) {
                            break;
                        }
                    } else {
                        unreachable!();
                    }
                }

                // the last chunk indicates finish reason and optionally usage
                let last = ChatCompletionChunk {
                    id: respond_id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created: now_unix_s() as u64,
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
                ChatElement::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reasoning_output(p: &ChatResponseParser) -> String {
        p.output()
            .iter()
            .filter_map(|e| match e {
                ChatElement::Reasoning(s) => Some(s.as_str()),
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
            ChatElement::Call(fc) => {
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
            ChatElement::Call(fc) => {
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
            ChatElement::Text(s) => assert_eq!(s, ""),
            _ => panic!("expected Text element"),
        }
        match &p.output()[1] {
            ChatElement::Reasoning(s) => assert_eq!(s, "reasoning"),
            _ => panic!("expected Reasoning element"),
        }
        match &p.output()[2] {
            ChatElement::Call(fc) => {
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
            ChatElement::Call(fc) => {
                assert_eq!(fc.function.name, "<think>");
            }
            _ => panic!("expected Call element"),
        }
    }
}
