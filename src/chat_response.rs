// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use liter_llm::FunctionCall;

#[derive(Debug)]
pub enum ChatElement {
    Text(String),
    Reasoning(String),
    Call(FunctionCall),
}

pub struct ChatResponseParser {
    state: State,
    out: Vec<ChatElement>,
}

#[derive(Debug, PartialEq)]
enum State {
    Text,
    Thinking,
    MaybeCall(String),
}

impl ChatResponseParser {
    pub fn new() -> Self {
        Self {
            state: State::Text,
            out: Vec::new(),
        }
    }

    /// Push a chunk of streaming text into the parser.
    pub fn push_str(&mut self, input: &str) {
        let mut remaining = input;

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
                        .push(ChatElement::Text(remaining[..pos].to_string()));
                    remaining = &remaining[pos + Tag::Think.as_str().len()..];
                }
                (Some((pos, Tag::ToolCall)), State::Text) => {
                    self.state = State::MaybeCall(remaining[..pos].to_string());
                    remaining = &remaining[pos + Tag::ToolCall.as_str().len()..];
                }
                (Some((pos, Tag::CloseThink)), State::Thinking) => {
                    self.state = State::Text;
                    self.out
                        .push(ChatElement::Reasoning(remaining[..pos].to_string()));
                    remaining = &remaining[pos + "</think>".len()..];
                }
                (Some((pos, Tag::CloseToolCall)), State::MaybeCall(s)) => {
                    s.push_str(&remaining[..pos]);
                    remaining = &remaining[pos + "</tool_call>".len()..];

                    if let Ok(fc) = serde_json::from_str::<FunctionCall>(s) {
                        self.out.push(ChatElement::Call(fc));
                    } else {
                        // not a valid function call
                        self.out.push(ChatElement::Text(std::mem::take(s)));
                    }

                    self.state = State::Text;
                }

                // STATE AS-IS
                (Some((pos, t)), State::Text) => {
                    let p = pos + t.as_str().len();
                    self.out.push(ChatElement::Text(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::Thinking) => {
                    let p = pos + t.as_str().len();
                    self.out
                        .push(ChatElement::Reasoning(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::MaybeCall(s)) => {
                    let p = pos + t.as_str().len();
                    s.push_str(&remaining[..p]);
                    remaining = &remaining[p..];
                }

                // NO TAG
                (None, State::Text) => {
                    self.out.push(ChatElement::Text(remaining.to_string()));
                    remaining = "";
                }
                (None, State::Thinking) => {
                    self.out.push(ChatElement::Reasoning(remaining.to_string()));
                    remaining = "";
                }
                (None, State::MaybeCall(s)) => {
                    s.push_str(remaining);
                    remaining = "";
                }
            }
        }
    }

    pub fn output(&self) -> &[ChatElement] {
        &self.out
    }

    pub fn state(&self) -> &str {
        match self.state {
            State::Text => "Text",
            State::Thinking => "Thinking",
            State::MaybeCall(_) => "MaybeCall",
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
                assert_eq!(fc.name, "get_weather");
                assert_eq!(fc.arguments, "{}");
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
                assert_eq!(fc.name, "search");
                assert_eq!(fc.arguments, "{}");
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
                assert_eq!(fc.name, "calc");
                assert_eq!(fc.arguments, "{}");
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
                assert_eq!(fc.name, "<think>");
            }
            _ => panic!("expected Call element"),
        }
    }
}
