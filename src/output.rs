use std::collections::VecDeque;

use async_openai::types::{
    chat::{
        ChatChoice, ChatChoiceStream, ChatCompletionMessageToolCall,
        ChatCompletionMessageToolCallChunk, ChatCompletionMessageToolCalls,
        ChatCompletionResponseMessage, ChatCompletionStreamResponseDelta, CompletionUsage,
        CreateChatCompletionResponse, CreateChatCompletionStreamResponse, FinishReason,
        FunctionCall, FunctionCallStream, FunctionType, Role,
    },
    responses::{
        AssistantRole, FunctionToolCall, InputTokenDetails, OutputItem, OutputMessage,
        OutputMessageContent, OutputStatus, OutputTextContent, OutputTokenDetails, ReasoningItem,
        Response, ResponseUsage, Status, SummaryPart, SummaryTextContent,
    },
};
use uuid::Uuid;

use crate::{
    class_llama_engine::{ERR_CANCELLED, ERR_ENGINE_SHUTDOWN},
    engine::{EngineReply, StopReason},
    helpers::now_unix_s,
};

#[derive(Debug)]
pub(crate) enum OutputPart {
    Text(String),
    Call(ChatCompletionMessageToolCall),
    Reasoning(String),
}

impl OutputPart {
    pub fn into_chat_chunk_delta(self) -> serde_json::Value {
        match self {
            Self::Text(t) => serde_json::to_value(ChatCompletionStreamResponseDelta {
                content: Some(t),
                role: None,
                tool_calls: None,
                refusal: None,
                function_call: None,
            })
            .unwrap(),

            Self::Reasoning(t) => {
                let mut delta = serde_json::to_value(ChatCompletionStreamResponseDelta {
                    content: None,
                    role: None,
                    tool_calls: None,
                    refusal: None,
                    function_call: None,
                })
                .unwrap();
                let obj = delta.as_object_mut().unwrap();
                obj.insert("reasoning_content".to_string(), t.into());

                delta
            }

            Self::Call(tool_call) => serde_json::to_value(ChatCompletionStreamResponseDelta {
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
            })
            .unwrap(),
        }
    }

    pub fn into_chat_chunk(self, id: &str, model: &str, created: u32) -> serde_json::Value {
        let delta = self.into_chat_chunk_delta();
        let mut chunk = serde_json::to_value(CreateChatCompletionStreamResponse {
            id: id.to_string(),
            model: model.to_string(),
            created,
            object: "chat.completion.chunk".to_string(),
            choices: vec![ChatChoiceStream {
                // an empty placeholder for delta
                delta: ChatCompletionStreamResponseDelta {
                    content: None,
                    role: None,
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
        })
        .unwrap();

        chunk["choices"][0]["delta"] = delta; // update to real delta
        chunk
    }
}

pub(crate) enum OutputChunk {
    Part(OutputPart),
    Stop(FinishReason, Option<CompletionUsage>),
    Done,
    Error(String),
}

impl OutputChunk {
    pub fn into_chat_chunk(self, id: &str, model: &str, created: u32) -> serde_json::Value {
        match self {
            Self::Part(part) => part.into_chat_chunk(id, model, created),
            Self::Stop(finish_reason, usage) => {
                serde_json::to_value(CreateChatCompletionStreamResponse {
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
                })
                .unwrap()
            }
            Self::Done | Self::Error(_) => {
                unreachable!();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum OutputTag {
    Think,
    CloseThink,
    ToolCall,
    CloseToolCall,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ParserKind {
    Chat,
    Responses,
}

#[derive(Debug, PartialEq)]
enum State {
    Text,
    MaybeCall(String),
    Thinking,
}

pub struct LlmOutputParser {
    /// current state
    state: State,
    /// parsed output parts
    parts: VecDeque<OutputPart>,
    parse_think: bool,
    n_prompt_tokens: usize,
    respond_id: String,
    model_name: String,
    include_usage: bool,
    think_tags: (String, String),
    tool_call_tags: (String, String),
}

impl LlmOutputParser {
    /// start_thinking: whether initial parser state is thinking or not.
    pub fn new(parse_think: bool, start_thinking: bool) -> Self {
        Self {
            parse_think,
            state: if start_thinking {
                State::Thinking
            } else {
                State::Text
            },
            parts: VecDeque::new(),
            n_prompt_tokens: 0,
            include_usage: false,
            respond_id: String::new(),
            model_name: String::new(),
            think_tags: (String::from("<think>"), String::from("</think>")),
            tool_call_tags: (String::from("<tool_call>"), String::from("</tool_call>")),
        }
    }

    pub fn set_think_tags(&mut self, tags: (&str, &str)) -> &mut Self {
        self.think_tags = (tags.0.to_string(), tags.1.to_string());
        self
    }

    pub fn set_tool_call_tags(&mut self, tags: (&str, &str)) -> &mut Self {
        self.tool_call_tags = (tags.0.to_string(), tags.1.to_string());
        self
    }

    pub fn set_respond_id(&mut self, id: String) -> &mut Self {
        self.respond_id = id;
        self
    }

    pub fn set_model_name(&mut self, name: String) -> &mut Self {
        self.model_name = name;
        self
    }

    pub fn set_n_prompt_tokens(&mut self, n: usize) -> &mut Self {
        self.n_prompt_tokens = n;
        self
    }

    pub fn set_include_usage(&mut self, include_usage: bool) -> &mut Self {
        self.include_usage = include_usage;
        self
    }

    pub fn n_prompt_tokens(&self) -> usize {
        self.n_prompt_tokens
    }

    /// get tag text length
    fn tag_len(&self, t: OutputTag) -> usize {
        match t {
            OutputTag::Think => self.think_tags.0.len(),
            OutputTag::CloseThink => self.think_tags.1.len(),
            OutputTag::ToolCall => self.tool_call_tags.0.len(),
            OutputTag::CloseToolCall => self.tool_call_tags.1.len(),
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
                    tool_call_pos = remaining.find(self.tool_call_tags.0.as_str());
                    if self.parse_think {
                        think_pos = remaining.find(self.think_tags.0.as_str());
                    }
                }
                State::MaybeCall(_) => {
                    close_tool_call_pos = remaining.find(self.tool_call_tags.1.as_str());
                }
                State::Thinking => {
                    close_think_pos = remaining.find(self.think_tags.1.as_str());
                }
            }

            let mut found_tag: Option<(usize, OutputTag)> = None;

            for (p, t) in [
                (think_pos, OutputTag::Think),
                (close_think_pos, OutputTag::CloseThink),
                (tool_call_pos, OutputTag::ToolCall),
                (close_tool_call_pos, OutputTag::CloseToolCall),
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
                // think
                (Some((pos, OutputTag::Think)), State::Text) => {
                    self.state = State::Thinking;
                    if pos > 0 {
                        self.parts
                            .push_back(OutputPart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + self.think_tags.0.len()..];
                }
                (Some((pos, OutputTag::CloseThink)), State::Thinking) => {
                    self.state = State::Text;
                    if pos > 0 {
                        self.parts
                            .push_back(OutputPart::Reasoning(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + self.think_tags.1.len()..];
                }

                // tool call
                (Some((pos, OutputTag::ToolCall)), State::Text) => {
                    if pos > 0 {
                        self.parts
                            .push_back(OutputPart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + self.tool_call_tags.0.len()..];
                    self.state = State::MaybeCall(String::new());
                }
                (Some((pos, OutputTag::CloseToolCall)), State::MaybeCall(s)) => {
                    s.push_str(&remaining[..pos]);
                    remaining = &remaining[pos + self.tool_call_tags.1.len()..];

                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(s) {
                        let obj = match value.as_object() {
                            Some(obj) if obj.len() == 2 => obj,
                            _ => {
                                self.parts.push_back(OutputPart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        let name = match obj.get("name").and_then(|v| v.as_str()) {
                            Some(n) => n.to_string(),
                            None => {
                                self.parts.push_back(OutputPart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        let arguments = match obj.get("arguments") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(other) => serde_json::to_string(other).unwrap_or_default(),
                            None => {
                                self.parts.push_back(OutputPart::Text(std::mem::take(s)));
                                return;
                            }
                        };

                        self.parts
                            .push_back(OutputPart::Call(ChatCompletionMessageToolCall {
                                id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                                function: FunctionCall { name, arguments },
                            }));
                    } else {
                        self.parts.push_back(OutputPart::Text(std::mem::take(s)));
                    }

                    self.state = State::Text;
                }

                // start think or tool_call
                (Some((pos, t)), State::Text) => {
                    debug_assert!(matches!(t, OutputTag::Think | OutputTag::ToolCall));
                    if pos > 0 {
                        self.parts
                            .push_back(OutputPart::Text(remaining[..pos].to_string()));
                    }
                    remaining = &remaining[pos + self.tag_len(t)..];
                    self.state = if t == OutputTag::Think {
                        State::Thinking
                    } else {
                        State::Text
                    };
                }

                (Some((_, t)), State::Thinking | State::MaybeCall(_)) => {
                    unreachable!("impossible tag: {t:?}");
                }

                (None, State::Text) => {
                    debug_assert!(!remaining.is_empty());
                    self.parts
                        .push_back(OutputPart::Text(remaining.to_string()));
                    remaining = "";
                }
                (None, State::Thinking) => {
                    debug_assert!(!remaining.is_empty());
                    self.parts
                        .push_back(OutputPart::Reasoning(remaining.to_string()));
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
            self.parts
                .push_back(OutputPart::Text(format!("{}{s}", &self.tool_call_tags.0)));
        }
    }

    pub fn next(&mut self) -> Option<OutputPart> {
        self.parts.pop_front()
    }

    pub(crate) fn parse_partial<F>(
        mut self,
        rx: crossbeam::channel::Receiver<EngineReply>,
        timeout: std::time::Duration,
        mut on_item: F,
    ) where
        F: FnMut(OutputChunk, usize) -> bool,
    {
        debug_assert!(self.parts.is_empty());

        if self.state == State::Thinking && !self.parse_think {
            // change init state to text
            self.parts
                .push_back(OutputPart::Text(self.think_tags.0.clone()));
            self.state = State::Text;
        }

        let mut nth = 0_usize; // index of chunk
        let mut has_tool_call = false;

        loop {
            match rx.recv_timeout(timeout) {
                Ok(EngineReply::Text(t)) => {
                    self.push_str(&t);
                    while let Some(part) = self.next() {
                        if matches!(part, OutputPart::Call(_)) {
                            has_tool_call = true;
                        }

                        if !on_item(OutputChunk::Part(part), nth) {
                            break;
                        }
                        nth += 1;
                    }
                }
                Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                    self.end();
                    while let Some(part) = self.next() {
                        if !on_item(OutputChunk::Part(part), nth) {
                            break;
                        }
                        nth += 1;
                    }

                    // emit stop message
                    let finish_reason = match reason {
                        StopReason::Eog | StopReason::Word(_) | StopReason::PrefillOnly => {
                            if has_tool_call {
                                FinishReason::ToolCalls
                            } else {
                                FinishReason::Stop
                            }
                        }
                        StopReason::MaxTokens => FinishReason::Length,
                    };

                    let usage = if self.include_usage {
                        Some(CompletionUsage {
                            prompt_tokens: self.n_prompt_tokens as u32,
                            completion_tokens: n_completion_tokens as u32,
                            total_tokens: (self.n_prompt_tokens + n_completion_tokens) as u32,
                            completion_tokens_details: None,
                            prompt_tokens_details: None,
                        })
                    } else {
                        None
                    };

                    on_item(OutputChunk::Stop(finish_reason, usage), nth);
                    nth += 1;

                    // emit done message
                    on_item(OutputChunk::Done, nth);
                    break;
                }
                Ok(EngineReply::Cancelled) => {
                    on_item(OutputChunk::Error(ERR_CANCELLED.to_string()), nth);
                    break;
                }
                Ok(EngineReply::Error(e)) => {
                    on_item(OutputChunk::Error(e), nth);
                    break;
                }
                Ok(EngineReply::Shutdown) => {
                    on_item(OutputChunk::Error(ERR_ENGINE_SHUTDOWN.to_string()), nth);
                    break;
                }
                Err(_) => {
                    on_item(OutputChunk::Error("stream timed out".to_string()), nth);
                    break;
                }
            }
        }
    }

    pub(crate) fn parse_all(
        mut self,
        rx: crossbeam::channel::Receiver<EngineReply>,
        timeout: std::time::Duration,
        kind: ParserKind,
    ) -> Result<serde_json::Value, String> {
        debug_assert!(self.parts.is_empty());

        if self.state == State::Thinking && !self.parse_think {
            // change init state to text
            self.parts
                .push_back(OutputPart::Text(self.think_tags.0.clone()));
            self.state = State::Text;
        }

        let tm = now_unix_s();

        loop {
            match rx.recv_timeout(timeout) {
                Ok(EngineReply::Text(t)) => {
                    self.push_str(&t);
                }
                Ok(EngineReply::Stop(reason, n_completion_tokens)) => {
                    self.end();

                    let mut text = String::new();
                    let mut reasoning = String::new();
                    let mut calls = Vec::<ChatCompletionMessageToolCalls>::new();

                    while let Some(part) = self.next() {
                        match part {
                            OutputPart::Text(t) => {
                                text.push_str(&t);
                            }
                            OutputPart::Call(t) => {
                                calls.push(ChatCompletionMessageToolCalls::Function(t));
                            }
                            OutputPart::Reasoning(t) => {
                                reasoning.push_str(&t);
                            }
                        }
                    }

                    match kind {
                        ParserKind::Chat => {
                            let finish_reason = match reason {
                                StopReason::MaxTokens => FinishReason::Length,
                                StopReason::PrefillOnly => FinishReason::Stop,
                                StopReason::Eog | StopReason::Word(_) => {
                                    if calls.is_empty() {
                                        FinishReason::ToolCalls
                                    } else {
                                        FinishReason::Stop
                                    }
                                }
                            };

                            let mut resp = serde_json::to_value(CreateChatCompletionResponse {
                                id: self.respond_id,
                                object: "chat.completion".to_string(),
                                created: now_unix_s() as u32,
                                model: self.model_name,
                                choices: vec![ChatChoice {
                                    index: 0,
                                    message: ChatCompletionResponseMessage {
                                        role: Role::Assistant,
                                        content: if !text.is_empty() { Some(text) } else { None },
                                        tool_calls: if !calls.is_empty() {
                                            Some(calls)
                                        } else {
                                            None
                                        },
                                        refusal: None,
                                        function_call: None,
                                        audio: None,
                                        annotations: None,
                                    },
                                    finish_reason: Some(finish_reason),
                                    logprobs: None,
                                }],
                                usage: if self.include_usage {
                                    Some(CompletionUsage {
                                        prompt_tokens: self.n_prompt_tokens as u32,
                                        completion_tokens: n_completion_tokens as u32,
                                        total_tokens: (self.n_prompt_tokens + n_completion_tokens)
                                            as u32,
                                        completion_tokens_details: None,
                                        prompt_tokens_details: None,
                                    })
                                } else {
                                    None
                                },
                                system_fingerprint: None,
                                service_tier: None,
                            })
                            .map_err(|e| e.to_string())?;

                            // add extra reasoning_content field
                            if !reasoning.is_empty()
                                && let Some(choices) = resp["choices"].as_array_mut()
                                && let Some(choice) = choices.first_mut()
                                && let Some(msg) = choice["message"].as_object_mut()
                            {
                                msg.insert(
                                    "reasoning_content".to_string(),
                                    serde_json::Value::String(reasoning),
                                );
                            }

                            return Ok(resp);
                        }

                        ParserKind::Responses => {
                            let mut output: Vec<OutputItem> = Vec::new();

                            if !reasoning.is_empty() {
                                output.push(OutputItem::Reasoning(ReasoningItem {
                                    id: Some(format!("think_{}", Uuid::new_v4().simple())),
                                    summary: vec![SummaryPart::SummaryText(SummaryTextContent {
                                        text: reasoning,
                                    })],
                                    content: None,
                                    encrypted_content: None,
                                    status: None,
                                }));
                            }

                            if !text.is_empty() {
                                output.push(OutputItem::Message(OutputMessage {
                                    id: format!("text_{}", Uuid::new_v4().simple()),
                                    role: AssistantRole::Assistant,
                                    status: OutputStatus::Completed,
                                    content: vec![OutputMessageContent::OutputText(
                                        OutputTextContent {
                                            text,
                                            annotations: vec![],
                                            logprobs: None,
                                        },
                                    )],
                                    phase: None,
                                }));
                            }

                            for tc in calls {
                                match tc {
                                    ChatCompletionMessageToolCalls::Function(f) => {
                                        output.push(OutputItem::FunctionCall(FunctionToolCall {
                                            id: Some(f.id.clone()),
                                            call_id: f.id,
                                            name: f.function.name,
                                            arguments: f.function.arguments,
                                            namespace: None,
                                            status: Some(OutputStatus::Completed),
                                        }));
                                    }
                                    ChatCompletionMessageToolCalls::Custom(_) => {}
                                }
                            }

                            let resp = Response {
                                id: self.respond_id,
                                object: "response".to_string(),
                                created_at: tm as _,
                                completed_at: Some(now_unix_s() as _),
                                status: Status::Completed,
                                model: self.model_name,
                                output,
                                usage: if self.include_usage {
                                    Some(ResponseUsage {
                                        input_tokens: self.n_prompt_tokens as u32,
                                        output_tokens: n_completion_tokens as u32,
                                        total_tokens: (self.n_prompt_tokens + n_completion_tokens)
                                            as u32,
                                        input_tokens_details: InputTokenDetails {
                                            cached_tokens: 0,
                                        },
                                        output_tokens_details: OutputTokenDetails {
                                            reasoning_tokens: 0,
                                        },
                                    })
                                } else {
                                    None
                                },
                                background: None,
                                billing: None,
                                conversation: None,
                                error: None,
                                incomplete_details: None,
                                instructions: None,
                                max_output_tokens: None,
                                metadata: None,
                                parallel_tool_calls: None,
                                previous_response_id: None,
                                prompt: None,
                                prompt_cache_key: None,
                                prompt_cache_retention: None,
                                reasoning: None,
                                safety_identifier: None,
                                service_tier: None,
                                temperature: None,
                                text: None,
                                tool_choice: None,
                                tools: None,
                                top_logprobs: None,
                                top_p: None,
                                truncation: None,
                            };

                            return serde_json::to_value(resp).map_err(|e| e.to_string());
                        }
                    }
                }
                Ok(EngineReply::Cancelled) => return Err(ERR_CANCELLED.to_string()),
                Ok(EngineReply::Error(e)) => return Err(e),
                Ok(EngineReply::Shutdown) => return Err(ERR_ENGINE_SHUTDOWN.to_string()),
                Err(_) => return Err("stream timed out".to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_output(p: &LlmOutputParser) -> String {
        p.parts
            .iter()
            .filter_map(|e| match e {
                OutputPart::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    fn reasoning_output(p: &LlmOutputParser) -> String {
        p.parts
            .iter()
            .filter_map(|e| match e {
                OutputPart::Reasoning(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_passthrough() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("Hello, world!");
        assert_eq!(text_output(&p), "Hello, world!");
    }

    #[test]
    fn think_tag_basic() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("Hello <think>hmm</think> world");
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), "Hello  world");
        assert_eq!(reasoning_output(&p), "hmm");
        assert_eq!(p.state, State::Text);
    }

    #[test]
    fn think_tag_multiple_chunks() {
        let mut p = LlmOutputParser::new(true, false);
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
        let mut p = LlmOutputParser::new(true, false);
        p.push_str(r#"<tool_call>{"name":"get_weather","arguments":"{}"}</tool_call>"#);
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), "");
        assert_eq!(p.parts.len(), 1);
        match &p.parts[0] {
            OutputPart::Call(fc) => {
                assert_eq!(fc.function.name, "get_weather");
                assert_eq!(fc.function.arguments, "{}");
            }
            _ => panic!("expected Call element"),
        }
    }

    #[test]
    fn tool_call_invalid_json_fallback() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("bad: <tool_call>{invalid}</tool_call>");
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), "bad: {invalid}");
    }

    #[test]
    fn tool_call_partial_then_invalid() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("start <tool_call>{\"key\"");
        assert_eq!(p.state, State::MaybeCall("{\"key\"".to_string()));
        assert_eq!(text_output(&p), "start ");
        p.push_str(": invalid}");
        assert_eq!(p.state, State::MaybeCall("{\"key\": invalid}".to_string()));
    }

    #[test]
    fn tool_call_multiple_chunks_valid() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str(r#"<tool_call>{"name":"#);
        assert_eq!(p.state, State::MaybeCall(r#"{"name":"#.to_string()));
        assert_eq!(text_output(&p), "");
        p.push_str(r#""search""#);
        p.push_str(r#","arguments":"{}"}</tool_call> b"#);
        assert_eq!(p.state, State::Text);
        assert_eq!(text_output(&p), " b");
        assert_eq!(p.parts.len(), 2);
        match &p.parts[0] {
            OutputPart::Call(fc) => {
                assert_eq!(fc.function.name, "search");
                assert_eq!(fc.function.arguments, "{}");
            }
            _ => panic!("expected Call element"),
        }
    }

    #[test]
    fn mixed_think_and_tool_call() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str(
            r#"<think>reasoning</think> <tool_call>{"name":"calc","arguments":"{}"}</tool_call>"#,
        );
        assert_eq!(reasoning_output(&p), "reasoning");
        assert_eq!(text_output(&p), " ");
        assert_eq!(p.parts.len(), 3);
        match &p.parts[0] {
            OutputPart::Reasoning(s) => assert_eq!(s, "reasoning"),
            _ => panic!("expected Think element"),
        }
        match &p.parts[1] {
            OutputPart::Text(s) => assert_eq!(s, " "),
            _ => panic!("expected Text element"),
        }
        match &p.parts[2] {
            OutputPart::Call(fc) => {
                assert_eq!(fc.function.name, "calc");
                assert_eq!(fc.function.arguments, "{}");
            }
            _ => panic!("expected Call element"),
        }
    }

    #[test]
    fn no_tags() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("just text");
        assert_eq!(text_output(&p), "just text");
    }

    #[test]
    fn empty_input() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("");
        assert!(p.parts.is_empty());
    }

    #[test]
    fn orphan_close_think_in_text() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("hello </think> world");
        assert_eq!(text_output(&p), "hello </think> world");
    }

    #[test]
    fn orphan_close_tool_call_in_text() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str("hello </tool_call> world");
        assert_eq!(text_output(&p), "hello </tool_call> world");
    }

    #[test]
    fn tool_call_nested_think_inside() {
        let mut p = LlmOutputParser::new(true, false);
        p.push_str(r#"<tool_call>{"name":"<think>","arguments":"{}"}</tool_call>"#);
        assert_eq!(p.parts.len(), 1);
        match &p.parts[0] {
            OutputPart::Call(fc) => {
                assert_eq!(fc.function.name, "<think>");
            }
            _ => panic!("expected Call element"),
        }
    }
}
