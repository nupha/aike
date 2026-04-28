// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! LLM request client built on liter-llm with OpenAI-compatible protocol.
//!
//! Provides a thin wrapper around `liter-llm::DefaultClient` for sending
//! chat completion requests (unary and streaming) to any OpenAI-compatible
//! endpoint.

use std::time::Duration;

use anyhow::Result;
use liter_llm::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ClientConfigBuilder,
    DefaultClient, EmbeddingInput, EmbeddingRequest, LlmClient, ReasoningEffort,
    types::chat::StreamOptions,
    types::common::{
        AssistantMessage, ChatCompletionTool, Message, ResponseFormat, StopSequence, SystemMessage,
        ToolChoice, UserContent, UserMessage,
    },
};
use serde_json::Value;

/// Configuration for creating an [`LlmRequestClient`].
pub struct ClientOpts {
    /// API key for authentication.
    pub api_key: String,
    /// Base URL of the OpenAI-compatible endpoint, e.g. "http://localhost:8080/v1".
    pub base_url: String,
    /// Default model name, e.g. "qwen3.5-4b".
    pub model: String,
    /// Request timeout. Defaults to 120s.
    pub timeout: Duration,
    /// Max retries on 429 / 5xx. Defaults to 0.
    pub max_retries: u32,
}

impl Default for ClientOpts {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: String::from("http://localhost:8080/v1"),
            model: String::from("default"),
            timeout: Duration::from_secs(120),
            max_retries: 0,
        }
    }
}

/// OpenAI-protocol LLM request client.
///
/// Wraps `liter-llm::DefaultClient` and exposes convenient chat methods.
pub struct LlmRequestClient {
    client: DefaultClient,
    model: String,
}

impl LlmRequestClient {
    /// Build a new client from [`ClientOpts`].
    pub fn new(opts: ClientOpts) -> Result<Self> {
        let config = ClientConfigBuilder::new(&opts.api_key)
            .base_url(&opts.base_url)
            .timeout(opts.timeout)
            .max_retries(opts.max_retries)
            .build();

        let client = DefaultClient::new(config, None)?;
        Ok(Self {
            client,
            model: opts.model,
        })
    }

    /// Build a client targeting a local llama-js server.
    pub fn local(port: u16, model: &str) -> Result<Self> {
        Self::new(ClientOpts {
            api_key: "no-key".into(),
            base_url: format!("http://localhost:{port}/v1"),
            model: model.into(),
            ..Default::default()
        })
    }

    /// Send a chat completion request.
    pub async fn chat(
        &self,
        messages: Vec<Message>,
        params: ChatParams,
    ) -> Result<ChatCompletionResponse> {
        let req = params.build_request(self.model.clone(), messages);
        let resp = self.client.chat(req).await?;
        Ok(resp)
    }

    /// Simple one-shot: system prompt + user prompt → assistant text.
    pub async fn ask(&self, system: &str, user: &str) -> Result<String> {
        let messages = vec![
            Message::System(SystemMessage {
                content: system.into(),
                name: None,
            }),
            Message::User(UserMessage {
                content: UserContent::Text(user.into()),
                name: None,
            }),
        ];
        let resp = self.chat(messages, ChatParams::default()).await?;
        let text = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        Ok(text)
    }

    /// Send a streaming chat completion request.
    pub async fn chat_stream(
        &self,
        messages: Vec<Message>,
        params: ChatParams,
    ) -> Result<liter_llm::BoxStream<'static, Result<ChatCompletionChunk, liter_llm::LiterLlmError>>>
    {
        let req = params.build_request(self.model.clone(), messages);
        let stream = self.client.chat_stream(req).await?;
        Ok(stream)
    }

    /// Collect a full streaming response into a single string.
    pub async fn chat_stream_collect(&self, messages: Vec<Message>) -> Result<String> {
        use futures_util::StreamExt;

        let stream = self.chat_stream(messages, ChatParams::default()).await?;
        let mut output = String::new();
        let mut stream = std::pin::pin!(stream);

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if let Some(choice) = chunk.choices.first() {
                if let Some(ref content) = choice.delta.content {
                    output.push_str(content);
                }
            }
        }

        Ok(output)
    }

    /// Generate embeddings for the given texts.
    pub async fn embed(&self, input: Vec<String>) -> Result<liter_llm::EmbeddingResponse> {
        let req = EmbeddingRequest {
            model: self.model.clone(),
            input: EmbeddingInput::Multiple(input),
            encoding_format: None,
            dimensions: None,
            user: None,
        };
        let resp = self.client.embed(req).await?;
        Ok(resp)
    }

    /// List models available on the remote endpoint.
    pub async fn list_models(&self) -> Result<liter_llm::ModelsListResponse> {
        let resp = self.client.list_models().await?;
        Ok(resp)
    }

    /// Return the default model name.
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// Optional parameters for chat completion requests.
///
/// Wraps all OpenAI-compatible optional fields; `Default::default()` gives empty/None
/// for everything, which is equivalent to the simple `chat()` call.
#[derive(Clone, Default)]
pub struct ChatParams {
    /// Sampling temperature (0–2). Higher = more random.
    temperature: Option<f64>,
    /// Nucleus sampling threshold.
    top_p: Option<f64>,
    /// Max tokens to generate.
    max_tokens: Option<u64>,
    /// Stop sequences.
    stop: Option<StopSequence>,
    /// Tool definitions for function calling.
    tools: Option<Vec<ChatCompletionTool>>,
    /// Controls tool selection behavior.
    tool_choice: Option<ToolChoice>,
    /// Allow parallel tool calls.
    parallel_tool_calls: Option<bool>,
    /// Reasoning / thinking effort (e.g. o1, deepseek-r1, qwen3).
    reasoning_effort: Option<ReasoningEffort>,
    /// Enforce a specific response format (e.g. JSON mode).
    response_format: Option<ResponseFormat>,
    /// Reproducible sampling seed.
    seed: Option<i64>,
    /// Presence penalty (−2–2).
    presence_penalty: Option<f64>,
    /// Frequency penalty (−2–2).
    frequency_penalty: Option<f64>,
    /// Stream options (e.g. include_usage).
    stream_options: Option<StreamOptions>,
    /// End-user identifier for abuse monitoring.
    user: Option<String>,
    /// Provider-specific extra body merged into the request JSON.
    extra_body: Option<Value>,
}

impl ChatParams {
    // Builder-style chainable setters

    /// Set sampling temperature.
    pub fn temperature(mut self, v: f64) -> Self {
        self.temperature = Some(v);
        self
    }

    /// Set nucleus sampling threshold.
    pub fn top_p(mut self, v: f64) -> Self {
        self.top_p = Some(v);
        self
    }

    /// Set max tokens to generate.
    pub fn max_tokens(mut self, v: u64) -> Self {
        self.max_tokens = Some(v);
        self
    }

    /// Set a single stop sequence.
    pub fn stop(mut self, s: impl Into<String>) -> Self {
        self.stop = Some(StopSequence::Single(s.into()));
        self
    }

    /// Set multiple stop sequences.
    pub fn stop_many(mut self, ss: Vec<String>) -> Self {
        self.stop = Some(StopSequence::Multiple(ss));
        self
    }

    /// Set tool definitions for function calling.
    pub fn tools(mut self, tools: Vec<ChatCompletionTool>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// Set tool choice behavior.
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Allow or disallow parallel tool calls.
    pub fn parallel_tool_calls(mut self, v: bool) -> Self {
        self.parallel_tool_calls = Some(v);
        self
    }

    /// Set reasoning / thinking effort.
    pub fn reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Set response format (e.g. JSON mode).
    pub fn response_format(mut self, fmt: ResponseFormat) -> Self {
        self.response_format = Some(fmt);
        self
    }

    /// Set reproducible sampling seed.
    pub fn seed(mut self, v: i64) -> Self {
        self.seed = Some(v);
        self
    }

    /// Set presence penalty.
    pub fn presence_penalty(mut self, v: f64) -> Self {
        self.presence_penalty = Some(v);
        self
    }

    /// Set frequency penalty.
    pub fn frequency_penalty(mut self, v: f64) -> Self {
        self.frequency_penalty = Some(v);
        self
    }

    /// Set stream options.
    pub fn stream_options(mut self, opts: StreamOptions) -> Self {
        self.stream_options = Some(opts);
        self
    }

    /// Set end-user identifier.
    pub fn user(mut self, v: impl Into<String>) -> Self {
        self.user = Some(v.into());
        self
    }

    /// Set provider-specific extra body.
    pub fn extra_body(mut self, v: Value) -> Self {
        self.extra_body = Some(v);
        self
    }

    /// Build a `ChatCompletionRequest` from this params + model + messages.
    pub fn build_request(self, model: String, messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model,
            messages,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_tokens,
            stop: self.stop,
            tools: self.tools,
            tool_choice: self.tool_choice,
            parallel_tool_calls: self.parallel_tool_calls,
            reasoning_effort: self.reasoning_effort,
            response_format: self.response_format,
            seed: self.seed,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            stream_options: self.stream_options,
            user: self.user,
            extra_body: self.extra_body,
            ..Default::default()
        }
    }
}

/// Build a user text message.
pub fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        name: None,
    })
}

/// Build a system message.
pub fn system_msg(text: &str) -> Message {
    Message::System(SystemMessage {
        content: text.into(),
        name: None,
    })
}

/// Build an assistant message (useful for multi-turn context).
pub fn assistant_msg(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: Some(text.into()),
        name: None,
        tool_calls: None,
        refusal: None,
        function_call: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_messages() {
        let msgs = vec![
            system_msg("You are helpful."),
            user_msg("Hello!"),
            assistant_msg("Hi there!"),
            user_msg("How are you?"),
        ];
        assert_eq!(msgs.len(), 4);
    }

    #[test]
    fn client_opts_default() {
        let opts = ClientOpts::default();
        assert_eq!(opts.base_url, "http://localhost:8080/v1");
        assert_eq!(opts.timeout, Duration::from_secs(120));
    }

    // --- ChatParams unit tests ---

    #[test]
    fn chat_params_default_all_none() {
        let req = ChatParams::default().build_request("test-model".into(), vec![user_msg("hi")]);
        assert_eq!(req.model, "test-model");
        assert_eq!(req.messages.len(), 1);
        assert!(req.temperature.is_none());
        assert!(req.top_p.is_none());
        assert!(req.max_tokens.is_none());
        assert!(req.stop.is_none());
        assert!(req.tools.is_none());
        assert!(req.tool_choice.is_none());
        assert!(req.parallel_tool_calls.is_none());
        assert!(req.reasoning_effort.is_none());
        assert!(req.response_format.is_none());
        assert!(req.seed.is_none());
        assert!(req.presence_penalty.is_none());
        assert!(req.frequency_penalty.is_none());
        assert!(req.stream_options.is_none());
        assert!(req.user.is_none());
        assert!(req.extra_body.is_none());
    }

    #[test]
    fn chat_params_temperature() {
        let req = ChatParams::default()
            .temperature(0.7)
            .build_request("m".into(), vec![]);
        assert_eq!(req.temperature, Some(0.7));
    }

    #[test]
    fn chat_params_top_p() {
        let req = ChatParams::default()
            .top_p(0.9)
            .build_request("m".into(), vec![]);
        assert_eq!(req.top_p, Some(0.9));
    }

    #[test]
    fn chat_params_max_tokens() {
        let req = ChatParams::default()
            .max_tokens(256)
            .build_request("m".into(), vec![]);
        assert_eq!(req.max_tokens, Some(256));
    }

    #[test]
    fn chat_params_stop_single() {
        let req = ChatParams::default()
            .stop("\n")
            .build_request("m".into(), vec![]);
        match req.stop {
            Some(StopSequence::Single(ref s)) => assert_eq!(s, "\n"),
            other => panic!("expected Single, got: {other:?}"),
        }
    }

    #[test]
    fn chat_params_stop_multiple() {
        let req = ChatParams::default()
            .stop_many(vec!["stop1".into(), "stop2".into()])
            .build_request("m".into(), vec![]);
        match req.stop {
            Some(StopSequence::Multiple(ref ss)) => {
                assert_eq!(ss.len(), 2);
                assert_eq!(ss[0], "stop1");
                assert_eq!(ss[1], "stop2");
            }
            other => panic!("expected Multiple, got: {other:?}"),
        }
    }

    #[test]
    fn chat_params_reasoning_effort_low() {
        let req = ChatParams::default()
            .reasoning_effort(ReasoningEffort::Low)
            .build_request("m".into(), vec![]);
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn chat_params_reasoning_effort_medium() {
        let req = ChatParams::default()
            .reasoning_effort(ReasoningEffort::Medium)
            .build_request("m".into(), vec![]);
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn chat_params_reasoning_effort_high() {
        let req = ChatParams::default()
            .reasoning_effort(ReasoningEffort::High)
            .build_request("m".into(), vec![]);
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn chat_params_seed() {
        let req = ChatParams::default()
            .seed(42)
            .build_request("m".into(), vec![]);
        assert_eq!(req.seed, Some(42));
    }

    #[test]
    fn chat_params_penalties() {
        let req = ChatParams::default()
            .presence_penalty(0.5)
            .frequency_penalty(-0.3)
            .build_request("m".into(), vec![]);
        assert_eq!(req.presence_penalty, Some(0.5));
        assert_eq!(req.frequency_penalty, Some(-0.3));
    }

    #[test]
    fn chat_params_parallel_tool_calls() {
        let req = ChatParams::default()
            .parallel_tool_calls(true)
            .build_request("m".into(), vec![]);
        assert_eq!(req.parallel_tool_calls, Some(true));
    }

    #[test]
    fn chat_params_user() {
        let req = ChatParams::default()
            .user("test-user")
            .build_request("m".into(), vec![]);
        assert_eq!(req.user, Some("test-user".into()));
    }

    #[test]
    fn chat_params_extra_body() {
        let body = serde_json::json!({"key": "value"});
        let req = ChatParams::default()
            .extra_body(body.clone())
            .build_request("m".into(), vec![]);
        assert_eq!(req.extra_body, Some(body));
    }

    #[test]
    fn chat_params_chained_builder() {
        let req = ChatParams::default()
            .temperature(0.5)
            .top_p(0.8)
            .max_tokens(1024)
            .stop("END")
            .seed(123)
            .presence_penalty(0.1)
            .frequency_penalty(0.2)
            .parallel_tool_calls(false)
            .user("u")
            .reasoning_effort(ReasoningEffort::Medium)
            .build_request("my-model".into(), vec![user_msg("hello")]);

        assert_eq!(req.model, "my-model");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.top_p, Some(0.8));
        assert_eq!(req.max_tokens, Some(1024));
        assert!(req.stop.is_some());
        assert_eq!(req.seed, Some(123));
        assert_eq!(req.presence_penalty, Some(0.1));
        assert_eq!(req.frequency_penalty, Some(0.2));
        assert_eq!(req.parallel_tool_calls, Some(false));
        assert_eq!(req.user, Some("u".into()));
        assert_eq!(req.reasoning_effort, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn chat_params_clone_preserves_fields() {
        let original = ChatParams::default().temperature(0.3).max_tokens(64);
        let cloned = original.clone();
        let req = cloned.build_request("m".into(), vec![]);
        assert_eq!(req.temperature, Some(0.3));
        assert_eq!(req.max_tokens, Some(64));
    }

    #[test]
    fn chat_params_override_by_rebuilding() {
        // Calling the same setter twice overwrites the previous value
        let req = ChatParams::default()
            .temperature(0.1)
            .temperature(0.9)
            .build_request("m".into(), vec![]);
        assert_eq!(req.temperature, Some(0.9));
    }

    // --- Integration tests (require running server) ---
    // Run with: cargo test -- --ignored

    /// Build a client targeting localhost:8080.
    /// Set env OPENAI_MODEL to override the model name.
    fn make_client() -> LlmRequestClient {
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "default".into());
        LlmRequestClient::local(8080, &model).expect("failed to create LlmRequestClient")
    }

    /// Check if the local server is reachable; skip test if not.
    async fn ensure_server(client: &LlmRequestClient) {
        if client.list_models().await.is_err() {
            eprintln!("SKIP: server not reachable at localhost:8080");
            std::process::exit(0);
        }
    }

    #[tokio::test]
    #[ignore] // requires running llama-js server on localhost:8080
    async fn chat_non_stream() {
        let client = make_client();
        ensure_server(&client).await;

        let messages = vec![
            system_msg("You are a helpful assistant. Reply concisely."),
            user_msg("Answer in one sentence: what is 1+1?"),
        ];

        let resp = client
            .chat(
                messages,
                ChatParams::default().temperature(0.2).max_tokens(32),
            )
            .await
            .expect("chat request failed");

        // Verify response structure
        assert!(!resp.id.is_empty(), "response id should not be empty");
        assert!(!resp.model.is_empty(), "response model should not be empty");
        assert_eq!(resp.choices.len(), 1, "should have exactly one choice");

        let choice = &resp.choices[0];
        let content = choice.message.content.as_deref().unwrap_or_default();
        assert!(!content.is_empty(), "assistant content should not be empty");

        let reason = choice
            .finish_reason
            .as_ref()
            .map(|r| format!("{r:?}"))
            .unwrap_or_default();
        assert!(
            reason == "Stop" || reason == "Length",
            "finish_reason should be stop or length, got: {reason}"
        );

        println!("[chat_non_stream] content: {content}");
        println!("[chat_non_stream] finish_reason: {reason}");
    }

    #[tokio::test]
    #[ignore]
    async fn chat_stream() {
        let client = make_client();
        ensure_server(&client).await;

        let messages = vec![
            system_msg("You are a helpful assistant. Reply concisely."),
            user_msg("Answer in one sentence: what is 2+2?"),
        ];

        let stream = client
            .chat_stream(
                messages,
                ChatParams::default().temperature(0.2).max_tokens(32),
            )
            .await
            .expect("stream request failed");

        let mut saw_delta = false;
        let mut saw_finish = false;
        let mut collected = String::new();
        let mut stream = std::pin::pin!(stream);

        use futures_util::StreamExt;
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.expect("stream chunk error");
            assert!(!chunk.id.is_empty(), "chunk id should not be empty");
            assert!(!chunk.model.is_empty(), "chunk model should not be empty");
            assert_eq!(chunk.choices.len(), 1, "chunk should have one choice");

            let choice = &chunk.choices[0];
            if let Some(ref content) = choice.delta.content {
                saw_delta = true;
                collected.push_str(content);
            }

            if let Some(ref reason) = choice.finish_reason {
                let r = format!("{reason:?}");
                assert!(
                    r == "Stop" || r == "Length",
                    "finish_reason should be stop or length, got: {r}"
                );
                saw_finish = true;
            }
        }

        assert!(saw_delta, "should receive at least one content delta");
        assert!(saw_finish, "should receive a finish_reason");
        assert!(!collected.is_empty(), "collected text should not be empty");

        println!("[chat_stream] collected: {collected}");
    }

    #[tokio::test]
    #[ignore]
    async fn ask_one_shot() {
        let client = make_client();
        ensure_server(&client).await;

        let text = client
            .ask(
                "You are a helpful assistant. Reply concisely.",
                "Answer in one word: what color is the sky on a clear day?",
            )
            .await
            .expect("ask request failed");

        assert!(!text.is_empty(), "ask() should return non-empty text");
        println!("[ask_one_shot] answer: {text}");
    }

    #[tokio::test]
    #[ignore]
    async fn chat_stream_collect() {
        let client = make_client();
        ensure_server(&client).await;

        let messages = vec![user_msg("Say hello in 5 words or less.")];
        let text = client
            .chat_stream_collect(messages)
            .await
            .expect("stream_collect request failed");

        assert!(
            !text.is_empty(),
            "stream_collect should return non-empty text"
        );
        println!("[chat_stream_collect] answer: {text}");
    }
}
