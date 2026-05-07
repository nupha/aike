// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use liter_llm::types::common::{
    AssistantMessage, Message, SystemMessage, UserContent, UserMessage,
};

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

/// Extract thinking content enclosed in `<think>...</think>` tags from raw response text.
///
/// Returns `(thinking, answer)` where `thinking` is the content inside the
/// think tags (None if absent) and `answer` is the text after the closing tag.
/// This is a manual helper — liter-llm does NOT parse think tags automatically.
pub fn extract_think(raw: &str) -> (Option<String>, &str) {
    let open = "<think>";
    let close = "</think>";
    let Some(start) = raw.find(open) else {
        return (None, raw);
    };
    let after_open = &raw[start + open.len()..];
    let Some(end) = after_open.find(close) else {
        return (None, raw);
    };
    let thinking = after_open[..end].trim().to_string();
    let answer = after_open[end + close.len()..].trim();
    (Some(thinking), answer)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use liter_llm::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ClientConfigBuilder,
        DefaultClient, FinishReason, FunctionCall, LlmClient, ToolCall, ToolType,
    };

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

    // --- Response deserialization unit tests ---

    /// Simulate a response with tool_calls and verify automatic parsing.
    #[test]
    fn response_tool_calls_parsed() {
        let json = serde_json::json!({
            "id": "chatcmpl-001",
            "object": "chat.completion",
            "created": 1234567890u64,
            "model": "qwen3.5-4b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"Beijing\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 10,
                "total_tokens": 30
            }
        });

        let resp: ChatCompletionResponse =
            serde_json::from_value(json).expect("deserialize response");

        assert_eq!(resp.choices.len(), 1);
        let choice = &resp.choices[0];

        // tool_calls are automatically parsed
        let tc = choice
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls should be parsed");
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_abc");
        assert_eq!(tc[0].call_type, ToolType::Function);
        assert_eq!(tc[0].function.name, "get_weather");
        assert_eq!(tc[0].function.arguments, r#"{"city":"Beijing"}"#);

        // content is None when the model only calls tools
        assert!(choice.message.content.is_none());

        // finish_reason is ToolCalls
        assert_eq!(choice.finish_reason, Some(FinishReason::ToolCalls));
    }

    /// Simulate a response with multiple tool_calls.
    #[test]
    fn response_multiple_tool_calls() {
        let json = serde_json::json!({
            "id": "chatcmpl-002",
            "object": "chat.completion",
            "created": 1234567890u64,
            "model": "qwen3.5-4b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "get_weather",
                                "arguments": "{\"city\":\"Beijing\"}"
                            }
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "get_time",
                                "arguments": "{\"timezone\":\"Asia/Shanghai\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": null
        });

        let resp: ChatCompletionResponse =
            serde_json::from_value(json).expect("deserialize response");
        let tc = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tc.len(), 2);
        assert_eq!(tc[0].function.name, "get_weather");
        assert_eq!(tc[1].function.name, "get_time");
    }

    /// Simulate a response where model returns both content and tool_calls.
    #[test]
    fn response_content_and_tool_calls() {
        let json = serde_json::json!({
            "id": "chatcmpl-003",
            "object": "chat.completion",
            "created": 1234567890u64,
            "model": "qwen3.5-4b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Let me check the weather for you.",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"Shanghai\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let resp: ChatCompletionResponse =
            serde_json::from_value(json).expect("deserialize response");
        let msg = &resp.choices[0].message;
        // Both content and tool_calls are present
        assert_eq!(
            msg.content.as_deref(),
            Some("Let me check the weather for you.")
        );
        assert!(msg.tool_calls.is_some());
    }

    /// Thinking content (e.g. Qwen3 <think>...</think>) is NOT automatically
    /// parsed by liter-llm. It appears as raw text in the `content` field.
    /// The caller must extract it manually.
    #[test]
    fn response_thinking_content_in_content_field() {
        let json = serde_json::json!({
            "id": "chatcmpl-004",
            "object": "chat.completion",
            "created": 1234567890u64,
            "model": "qwen3.5-4b",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "<think>\n1+1 equals 2.\n</think>\n\nThe answer is 2."
                },
                "finish_reason": "stop"
            }]
        });

        let resp: ChatCompletionResponse =
            serde_json::from_value(json).expect("deserialize response");
        let msg = &resp.choices[0].message;

        // Thinking is NOT separated — it's raw text in content
        let content = msg.content.as_deref().unwrap();
        assert!(content.contains("<think>"));
        assert!(content.contains("</think>"));
        assert!(content.contains("The answer is 2."));

        // No tool_calls for a plain text response
        assert!(msg.tool_calls.is_none());
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
    }

    /// Demonstrate manual extraction of think tags from content.
    #[test]
    fn manual_think_tag_extraction() {
        let raw = "<think>\nStep 1: reason about the problem.\n</think>\n\nThe result is 42.";

        let (thinking, answer) = extract_think(raw);
        assert_eq!(
            thinking,
            Some("Step 1: reason about the problem.".to_string())
        );
        assert_eq!(answer, "The result is 42.");
    }

    #[test]
    fn manual_think_tag_no_think() {
        let raw = "Just a plain answer without thinking.";
        let (thinking, answer) = extract_think(raw);
        assert!(thinking.is_none());
        assert_eq!(answer, raw);
    }

    /// Verify streaming chunk with tool_calls delta parsing.
    #[test]
    fn stream_chunk_tool_calls_delta() {
        let json = serde_json::json!({
            "id": "chatcmpl-chunk-1",
            "object": "chat.completion.chunk",
            "created": 1234567890u64,
            "model": "qwen3.5-4b",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_xyz",
                        "type": "function",
                        "function": {
                            "name": "search",
                            "arguments": "{\"q\":\"rust\"}"
                        }
                    }]
                },
                "finish_reason": null
            }]
        });

        let chunk: ChatCompletionChunk = serde_json::from_value(json).expect("deserialize chunk");
        let stc = chunk.choices[0]
            .delta
            .tool_calls
            .as_ref()
            .expect("stream tool_calls should be parsed");
        assert_eq!(stc.len(), 1);
        assert_eq!(stc[0].index, 0);
        assert_eq!(stc[0].id.as_deref(), Some("call_xyz"));
        let func = stc[0].function.as_ref().unwrap();
        assert_eq!(func.name.as_deref(), Some("search"));
    }

    /// Verify that FinishReason variants are correctly deserialized.
    #[test]
    fn finish_reason_variants() {
        assert_eq!(
            serde_json::from_value::<FinishReason>(serde_json::json!("stop")).unwrap(),
            FinishReason::Stop
        );
        assert_eq!(
            serde_json::from_value::<FinishReason>(serde_json::json!("length")).unwrap(),
            FinishReason::Length
        );
        assert_eq!(
            serde_json::from_value::<FinishReason>(serde_json::from_str("\"tool_calls\"").unwrap())
                .unwrap(),
            FinishReason::ToolCalls
        );
        assert_eq!(
            serde_json::from_value::<FinishReason>(serde_json::json!("content_filter")).unwrap(),
            FinishReason::ContentFilter
        );
    }

    /// Verify AssistantMessage with tool_calls round-trips through serialization.
    #[test]
    fn assistant_message_tool_calls_roundtrip() {
        let msg = AssistantMessage {
            content: None,
            name: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_42".into(),
                call_type: ToolType::Function,
                function: FunctionCall {
                    name: "calculator".into(),
                    arguments: "{\"expr\":\"2+2\"}".into(),
                },
            }]),
            refusal: None,
            function_call: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AssistantMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_calls.unwrap()[0].function.name, "calculator");
    }

    /// Build a `DefaultClient` targeting a local OpenAI-compatible server.
    fn local_client(port: u16, model: &str) -> anyhow::Result<DefaultClient> {
        let config = ClientConfigBuilder::new("no-key")
            .base_url(&format!("http://localhost:{port}/v1"))
            .timeout(Duration::from_secs(120))
            .build();
        DefaultClient::new(config, Some(model)).map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Build a client targeting localhost:8080.
    /// Set env OPENAI_MODEL to override the model name.
    fn make_client() -> DefaultClient {
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "default".into());
        local_client(8080, &model).expect("failed to create DefaultClient")
    }

    /// Check if the local server is reachable; skip test if not.
    async fn ensure_server(client: &DefaultClient) {
        if client.list_models().await.is_err() {
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

        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "default".into());
        let resp = client
            .chat(ChatCompletionRequest {
                model,
                messages,
                temperature: Some(0.2),
                max_tokens: Some(32),
                ..Default::default()
            })
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

        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "default".into());
        let stream = client
            .chat_stream(ChatCompletionRequest {
                model,
                messages,
                temperature: Some(0.2),
                max_tokens: Some(32),
                ..Default::default()
            })
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

        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "default".into());
        let messages = vec![
            Message::System(SystemMessage {
                content: "You are a helpful assistant. Reply concisely.".into(),
                name: None,
            }),
            Message::User(UserMessage {
                content: UserContent::Text(
                    "Answer in one word: what color is the sky on a clear day?".into(),
                ),
                name: None,
            }),
        ];
        let resp = client
            .chat(ChatCompletionRequest {
                model,
                messages,
                ..Default::default()
            })
            .await
            .expect("ask request failed");
        let text = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        assert!(!text.is_empty(), "ask() should return non-empty text");
        println!("[ask_one_shot] answer: {text}");
    }

    #[tokio::test]
    #[ignore]
    async fn chat_stream_collect() {
        let client = make_client();
        ensure_server(&client).await;

        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "default".into());
        let messages = vec![user_msg("Say hello in 5 words or less.")];
        let stream = client
            .chat_stream(ChatCompletionRequest {
                model,
                messages,
                ..Default::default()
            })
            .await
            .expect("stream_collect request failed");
        let mut output = String::new();
        let mut stream = std::pin::pin!(stream);

        use futures_util::StreamExt;
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.expect("stream chunk error");
            if let Some(choice) = chunk.choices.first()
                && let Some(ref content) = choice.delta.content
            {
                output.push_str(content);
            }
        }
        let text = output;

        assert!(
            !text.is_empty(),
            "stream_collect should return non-empty text"
        );
        println!("[chat_stream_collect] answer: {text}");
    }
}
