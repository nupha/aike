use async_openai::types::{
    chat::{
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestDeveloperMessage, ChatCompletionRequestDeveloperMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionTool, FunctionObject,
    },
    responses::{
        EasyInputContent, InputContent, InputItem, InputRole, Item, MessageItem,
        OutputMessageContent, Role as ResponsesRole,
    },
};

/// Convert a Responses API InputItem to a ChatCompletionRequestMessage.
pub fn input_item_to_chat_message(item: &InputItem) -> Option<ChatCompletionRequestMessage> {
    match item {
        InputItem::EasyMessage(msg) => {
            let content = match &msg.content {
                EasyInputContent::Text(t) => t.clone(),
                EasyInputContent::ContentList(_) => return None,
            };
            Some(build_chat_message(&msg.role, content))
        }
        InputItem::Item(item) => match item {
            Item::Message(msg_item) => match msg_item {
                MessageItem::Input(input_msg) => {
                    let text = input_msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            InputContent::InputText(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.is_empty() {
                        return None;
                    }
                    Some(build_chat_message_from_input_role(&input_msg.role, text))
                }
                MessageItem::Output(output_msg) => {
                    let text = output_msg
                        .content
                        .iter()
                        .map(|c| match c {
                            OutputMessageContent::OutputText(t) => t.text.clone(),
                            OutputMessageContent::Refusal(r) => r.refusal.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.is_empty() {
                        return None;
                    }
                    Some(ChatCompletionRequestMessage::Assistant(
                        ChatCompletionRequestAssistantMessage {
                            content: Some(ChatCompletionRequestAssistantMessageContent::Text(text)),
                            name: None,
                            tool_calls: None,
                            function_call: None,
                            refusal: None,
                            audio: None,
                        },
                    ))
                }
            },
            _ => None,
        },
        InputItem::ItemReference(_) => None,
    }
}

fn build_chat_message(role: &ResponsesRole, content: String) -> ChatCompletionRequestMessage {
    match role {
        ResponsesRole::User => {
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(content),
                name: None,
            })
        }
        ResponsesRole::Assistant => {
            ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                content: Some(ChatCompletionRequestAssistantMessageContent::Text(content)),
                name: None,
                tool_calls: None,
                function_call: None,
                refusal: None,
                audio: None,
            })
        }
        ResponsesRole::System => {
            ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(content),
                name: None,
            })
        }
        ResponsesRole::Developer => {
            ChatCompletionRequestMessage::Developer(ChatCompletionRequestDeveloperMessage {
                content: ChatCompletionRequestDeveloperMessageContent::Text(content),
                name: None,
            })
        }
    }
}

fn build_chat_message_from_input_role(
    role: &InputRole,
    content: String,
) -> ChatCompletionRequestMessage {
    match role {
        InputRole::User => ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Text(content),
            name: None,
        }),
        InputRole::System => {
            ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(content),
                name: None,
            })
        }
        InputRole::Developer => {
            ChatCompletionRequestMessage::Developer(ChatCompletionRequestDeveloperMessage {
                content: ChatCompletionRequestDeveloperMessageContent::Text(content),
                name: None,
            })
        }
    }
}

/// Convert Responses API tools to chat completion tools format.
pub fn responses_tools_to_chat_tools(
    tools: &[async_openai::types::responses::Tool],
) -> Vec<async_openai::types::chat::ChatCompletionTools> {
    tools
        .iter()
        .filter_map(|t| match t {
            async_openai::types::responses::Tool::Function(f) => Some(
                async_openai::types::chat::ChatCompletionTools::Function(ChatCompletionTool {
                    function: FunctionObject {
                        name: f.name.clone(),
                        description: f.description.clone(),
                        parameters: f.parameters.clone(),
                        strict: f.strict,
                    },
                }),
            ),
            _ => None,
        })
        .collect()
}
