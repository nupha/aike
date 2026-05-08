use std::collections::VecDeque;

use async_openai::types::chat::{
    ChatChoice, ChatChoiceStream, ChatCompletionMessageToolCall,
    ChatCompletionMessageToolCallChunk, ChatCompletionMessageToolCalls,
    ChatCompletionResponseMessage, ChatCompletionStreamResponseDelta, CompletionUsage,
    CreateChatCompletionResponse, CreateChatCompletionStreamResponse, FinishReason, FunctionCall,
    FunctionCallStream, FunctionType, Role,
};

use crate::{
    class_llama_engine::{ERR_CANCELLED, ERR_ENGINE_SHUTDOWN},
    engine::{EngineReply, StopReason},
    helpers::now_unix_s,
};

#[derive(Debug)]
pub(crate) enum ChatResponsePart {
    Text(String),
    Think(String),
    Call(ChatCompletionMessageToolCall),
}

impl ChatResponsePart {
    pub fn into_chunk_delta(self) -> ChatCompletionStreamResponseDelta {
        match self {
            Self::Text(t) => ChatCompletionStreamResponseDelta {
                content: Some(t),
                role: None,
                tool_calls: None,
                refusal: None,
                function_call: None,
            },
            Self::Think(t) => ChatCompletionStreamResponseDelta {
                content: Some(t),
                role: None,
                tool_calls: None,
                refusal: None,
                function_call: None,
            },
            Self::Call(tool_call) => ChatCompletionStreamResponseDelta {
                content: None,
                role: None,
                tool_calls: Some(vec![ChatCompletionMessageToolCallChunk {
                    index: 0,
                    id: Some(tool_call.id),
                    r#type: Some(FunctionType::Function),
                    function: Some(FunctionCallStream {
                        name: Some(tool_call.function.name),
                        arguments: Some(tool_call.function.arguments),
                    }),
                }]),
                refusal: None,
                function_call: None,
            },
        }
    }

    pub fn into_chunk(
        self,
        id: &str,
        model: &str,
        created: u32,
    ) -> CreateChatCompletionStreamResponse {
        CreateChatCompletionStreamResponse {
            id: id.to_string(),
            model: model.to_string(),
            created,
            object: "chat.completion.chunk".to_string(),
            choices: vec![ChatChoiceStream {
                delta: self.into_chunk_delta(),
                index: 0,
                finish_reason: None,
                logprobs: None,
            }],
            system_fingerprint: None,
            service_tier: None,
            usage: None,
        }
    }
}

pub(crate) enum ChatStreamItem {
    Role(Role),
    Part(ChatResponsePart),
    Stop(FinishReason, Option<CompletionUsage>),
    Done,
    Error(String),
}

impl ChatStreamItem {
    pub fn into_chunk(
        self,
        id: &str,
        model: &str,
        created: u32,
    ) -> CreateChatCompletionStreamResponse {
        match self {
            Self::Part(part) => part.into_chunk(id, model, created),
            Self::Role(role) => CreateChatCompletionStreamResponse {
                id: id.to_string(),
                model: model.to_string(),
                object: "chat.completion.chunk".to_string(),
                created,
                choices: vec![ChatChoiceStream {
                    delta: ChatCompletionStreamResponseDelta {
                        role: Some(role),
                        content: None,
                        tool_calls: None,
                        refusal: None,
                        function_call: None,
                    },
                    index: 0,
                    finish_reason: None,
                    logprobs: None,
                }],
                system_fingerprint: None,
                service_tier: None,
                usage: None,
            },
            Self::Stop(finish_reason, usage) => CreateChatCompletionStreamResponse {
                id: id.to_string(),
                model: model.to_string(),
                created,
                object: "chat.completion.chunk".to_string(),
                choices: vec![ChatChoiceStream {
                    delta: ChatCompletionStreamResponseDelta {
                        content: None,
                        role: None,
                        tool_calls: None,
                        refusal: None,
                        function_call: None,
                    },
                    index: 0,
                    finish_reason: Some(finish_reason),
                    logprobs: None,
                }],
                usage,
                system_fingerprint: None,
                service_tier: None,
            },
            Self::Done | Self::Error(_) => {
                unreachable!();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ChatResponseTag {
    Think,
    CloseThink,
    ToolCall,
    CloseToolCall,
}

impl ChatResponseTag {
    pub const fn as_str(&self) -> &str {
        match self {
            ChatResponseTag::Think => "<think>",
            ChatResponseTag::CloseThink => "</think>",
            ChatResponseTag::ToolCall => "<tool_call>",
            ChatResponseTag::CloseToolCall => "</tool_call>",
        }
    }
}

#[derive(Debug, PartialEq)]
enum State {
    Text,
    Thinking,
    MaybeCall(String),
}

pub struct ChatResponseParser {
    state: State,
    parts: VecDeque<ChatResponsePart>,
}

impl ChatResponseParser {
    pub fn new() -> Self {
        Self {
            state: State::Text,
            parts: VecDeque::new(),
        }
    }

    /// Feed streaming text into the parser.
    pub fn push_str(&mut self, chunk: &str) {
        let mut remaining = chunk;

        while !remaining.is_empty() {
            let mut think_pos: Option<usize> = None;
            let mut close_think_pos: Option<usize> = None;
            let mut tool_call_pos: Option<usize> = None;
            let mut close_tool_call_pos: Option<usize> = None;

            match self.state {
                State::Text => {
                    think_pos = remaining.find(ChatResponseTag::Think.as_str());
                    tool_call_pos = remaining.find(ChatResponseTag::ToolCall.as_str());
                }
                State::Thinking => {
                    close_think_pos = remaining.find(ChatResponseTag::CloseThink.as_str());
                }
                State::MaybeCall(_) => {
                    close_tool_call_pos = remaining.find(ChatResponseTag::CloseToolCall.as_str());
                }
            }

            let mut found_tag: Option<(usize, ChatResponseTag)> = None;

            for (p, t) in [
                (think_pos, ChatResponseTag::Think),
                (close_think_pos, ChatResponseTag::CloseThink),
                (tool_call_pos, ChatResponseTag::ToolCall),
                (close_tool_call_pos, ChatResponseTag::CloseToolCall),
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
                (Some((pos, ChatResponseTag::Think)), State::Text) => {
                    self.state = State::Thinking;
                    if pos > 0 {
                        self.parts
                            .push_back(ChatResponsePart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + ChatResponseTag::Think.as_str().len()..];
                }
                (Some((pos, ChatResponseTag::CloseThink)), State::Thinking) => {
                    self.state = State::Text;
                    if pos > 0 {
                        self.parts
                            .push_back(ChatResponsePart::Think(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + ChatResponseTag::CloseThink.as_str().len()..];
                }
                (Some((pos, ChatResponseTag::ToolCall)), State::Text) => {
                    if pos > 0 {
                        self.parts
                            .push_back(ChatResponsePart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + ChatResponseTag::ToolCall.as_str().len()..];
                    self.state = State::MaybeCall(String::new());
                }
                (Some((pos, ChatResponseTag::CloseToolCall)), State::MaybeCall(s)) => {
                    s.push_str(&remaining[..pos]);
                    remaining = &remaining[pos + ChatResponseTag::CloseToolCall.as_str().len()..];

                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(s) {
                        let obj = match value.as_object() {
                            Some(obj) if obj.len() == 2 => obj,
                            _ => {
                                self.parts
                                    .push_back(ChatResponsePart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        let name = match obj.get("name").and_then(|v| v.as_str()) {
                            Some(n) => n.to_string(),
                            None => {
                                self.parts
                                    .push_back(ChatResponsePart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        let arguments = match obj.get("arguments") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(other) => serde_json::to_string(other).unwrap_or_default(),
                            None => {
                                self.parts
                                    .push_back(ChatResponsePart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        self.parts.push_back(ChatResponsePart::Call(
                            ChatCompletionMessageToolCall {
                                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                                function: FunctionCall { name, arguments },
                            },
                        ));
                    } else {
                        self.parts
                            .push_back(ChatResponsePart::Text(std::mem::take(s)));
                    }

                    self.state = State::Text;
                }
                (Some((pos, t)), State::Text) => {
                    let p = pos + t.as_str().len();
                    self.parts
                        .push_back(ChatResponsePart::Text(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::Thinking) => {
                    let p = pos + t.as_str().len();
                    self.parts
                        .push_back(ChatResponsePart::Think(remaining[..p].to_string()));
                    remaining = &remaining[p..];
                }
                (Some((pos, t)), State::MaybeCall(s)) => {
                    let p = pos + t.as_str().len();
                    s.push_str(&remaining[..p]);
                    remaining = &remaining[p..];
                }
                (None, State::Text) => {
                    debug_assert!(!remaining.is_empty());
                    self.parts
                        .push_back(ChatResponsePart::Text(remaining.to_string()));
                    remaining = "";
                }
                (None, State::Thinking) => {
                    debug_assert!(!remaining.is_empty());
                    self.parts
                        .push_back(ChatResponsePart::Think(remaining.to_string()));
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
            self.parts.push_back(ChatResponsePart::Text(format!(
                "{}{s}",
                ChatResponseTag::ToolCall.as_str()
            )));
        }
    }

    pub fn next(&mut self) -> Option<ChatResponsePart> {
        self.parts.pop_front()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_response_stream<F>(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    include_usage: bool,
    n_prompt_tokens: usize,
    mut parser: ChatResponseParser,
    mut on_item: F,
) where
    F: FnMut(ChatStreamItem) -> bool,
{
    let mut sent_started = false;

    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Text(t)) => {
                if !sent_started {
                    sent_started = true;
                    if !on_item(ChatStreamItem::Role(
                        async_openai::types::chat::Role::Assistant,
                    )) {
                        break;
                    }
                }

                parser.push_str(&t);

                while let Some(part) = parser.next() {
                    if !on_item(ChatStreamItem::Part(part)) {
                        break;
                    }
                }
            }
            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                parser.end();

                while let Some(part) = parser.next() {
                    if !on_item(ChatStreamItem::Part(part)) {
                        break;
                    }
                }

                on_item(ChatStreamItem::Stop(
                    match reason {
                        StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                            FinishReason::Stop
                        }
                        StopReason::MaxTokens => FinishReason::Length,
                    },
                    if include_usage {
                        Some(CompletionUsage {
                            prompt_tokens: n_prompt_tokens as u32,
                            completion_tokens: n_completion_tokens as u32,
                            total_tokens: (n_prompt_tokens + n_completion_tokens) as u32,
                            completion_tokens_details: None,
                            prompt_tokens_details: None,
                        })
                    } else {
                        None
                    },
                ));
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

/// Collect all EngineReply messages for non-streaming chat, building a CreateChatCompletionResponse json.
/// Uses ChatResponseParser to parse tool calls and reasoning content from the generated text.
#[allow(clippy::too_many_arguments)]
pub(crate) fn chat_response_once(
    rx: crossbeam::channel::Receiver<EngineReply>,
    timeout: std::time::Duration,
    respond_id: &str,
    model_name: &str,
    include_usage: bool,
    n_prompt_tokens: usize,
    mut parser: ChatResponseParser,
) -> Result<serde_json::Value, String> {
    loop {
        match rx.recv_timeout(timeout) {
            Ok(EngineReply::Text(t)) => {
                parser.push_str(&t);
            }

            Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                parser.end();

                let mut text = String::new();
                let mut tool_calls = Vec::<ChatCompletionMessageToolCalls>::new();

                while let Some(part) = parser.next() {
                    match part {
                        ChatResponsePart::Text(t) => {
                            text.push_str(&t);
                        }
                        ChatResponsePart::Call(t) => {
                            tool_calls.push(ChatCompletionMessageToolCalls::Function(t));
                        }
                        _ => {}
                    }
                }

                let finish_reason = match reason {
                    StopReason::MaxTokens => FinishReason::Length,
                    StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                        FinishReason::Stop
                    }
                };

                let resp = CreateChatCompletionResponse {
                    id: respond_id.to_string(),
                    object: "chat.completion".to_string(),
                    created: now_unix_s() as u32,
                    model: model_name.to_string(),
                    choices: vec![ChatChoice {
                        index: 0,
                        message: ChatCompletionResponseMessage {
                            content: if !text.is_empty() { Some(text) } else { None },
                            tool_calls: if !tool_calls.is_empty() {
                                Some(tool_calls)
                            } else {
                                None
                            },
                            role: Role::Assistant,
                            refusal: None,
                            function_call: None,
                            audio: None,
                            annotations: None,
                        },
                        finish_reason: Some(finish_reason),
                        logprobs: None,
                    }],
                    usage: if include_usage {
                        Some(CompletionUsage {
                            prompt_tokens: n_prompt_tokens as u32,
                            completion_tokens: n_completion_tokens as u32,
                            total_tokens: (n_prompt_tokens + n_completion_tokens) as u32,
                            completion_tokens_details: None,
                            prompt_tokens_details: None,
                        })
                    } else {
                        None
                    },
                    system_fingerprint: None,
                    service_tier: None,
                };

                return serde_json::to_value(resp).map_err(|e| e.to_string());
            }
            Ok(EngineReply::Cancelled) => return Err(ERR_CANCELLED.to_string()),
            Ok(EngineReply::Error(e)) => return Err(e),
            Ok(EngineReply::Shutdown) => return Err(ERR_ENGINE_SHUTDOWN.to_string()),
            Err(_) => return Err("stream timed out".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_output(p: &ChatResponseParser) -> String {
        p.parts
            .iter()
            .filter_map(|e| match e {
                ChatResponsePart::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reasoning_output(p: &ChatResponseParser) -> String {
        p.parts
            .iter()
            .filter_map(|e| match e {
                ChatResponsePart::Think(s) => Some(s.as_str()),
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
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), "Hello  world");
        assert_eq!(reasoning_output(&p), "hmm");
        assert_eq!(p.state, State::Text);
    }

    #[test]
    fn think_tag_multiple_chunks() {
        let mut p = ChatResponseParser::new();
        p.push_str("Hello <think>");
        assert_eq!(p.state, State::Thinking);
        assert_eq!(text_output(&p), "Hello ");
        p.push_str("let me think");
        p.push_str(" carefully");
        p.push_str("</think> done");
        assert_eq!(p.state, State::Text);
        assert_eq!(reasoning_output(&p), "let me think carefully");
        assert_eq!(text_output(&p), "Hello  done");
    }

    #[test]
    fn tool_call_valid_json() {
        let mut p = ChatResponseParser::new();
        p.push_str(r#"<tool_call>{"name":"get_weather","arguments":"{}"}</tool_call>"#);
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), "");
        assert_eq!(p.parts.len(), 1);
        match &p.parts[0] {
            ChatResponsePart::Call(fc) => {
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
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), "bad: {invalid}");
    }

    #[test]
    fn tool_call_partial_then_invalid() {
        let mut p = ChatResponseParser::new();
        p.push_str("start <tool_call>{\"key\"");
        assert_eq!(p.state, State::MaybeCall("{\"key\"".to_string()));
        assert_eq!(text_output(&p), "start ");
        p.push_str(": invalid}");
        assert_eq!(p.state, State::MaybeCall("{\"key\": invalid}".to_string()));
    }

    #[test]
    fn tool_call_multiple_chunks_valid() {
        let mut p = ChatResponseParser::new();
        p.push_str(r#"<tool_call>{"name":"#);
        assert_eq!(p.state, State::MaybeCall(r#"{"name":"#.to_string()));
        assert_eq!(text_output(&p), "");
        p.push_str(r#""search""#);
        p.push_str(r#","arguments":"{}"}</tool_call> b"#);
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), " b");
        assert_eq!(p.parts.len(), 2);
        match &p.parts[0] {
            ChatResponsePart::Call(fc) => {
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
        assert_eq!(text_output(&p), " ");
        assert_eq!(p.parts.len(), 3);
        match &p.parts[0] {
            ChatResponsePart::Think(s) => assert_eq!(s, "reasoning"),
            _ => panic!("expected Think element"),
        }
        match &p.parts[1] {
            ChatResponsePart::Text(s) => assert_eq!(s, " "),
            _ => panic!("expected Text element"),
        }
        match &p.parts[2] {
            ChatResponsePart::Call(fc) => {
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
        assert!(p.parts.is_empty());
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
        p.push_str(r#"<tool_call>{"name":"<think>","arguments":"{}"}</tool_call>"#);
        assert_eq!(p.parts.len(), 1);
        match &p.parts[0] {
            ChatResponsePart::Call(fc) => {
                assert_eq!(fc.function.name, "<think>");
            }
            _ => panic!("expected Call element"),
        }
    }
}
