use std::ffi::CString;

use async_openai::types::chat::{
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestDeveloperMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessageContent,
    ChatCompletionToolChoiceOption, ChatCompletionTools, ReasoningEffort, ResponseFormat,
    ToolChoiceOptions,
};
use llama_cpp_ffi::{LlamaModel, ToolChoice as LarkToolChoice};
use minijinja::Environment;
use minijinja_contrib::pycompat;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::engine::Token;

#[derive(Debug, Serialize)]
pub(crate) struct JinjaMessage {
    pub role: String,
    pub content: String,
}

/// Context variables for minijinja chat template rendering.
/// Mirrors the jinja2 context that llama.cpp server builds in
/// `oaicompat_chat_params_parse` (server-common.cpp L909-1170).
#[derive(Debug, Serialize)]
pub(crate) struct TemplateContext<'a> {
    pub messages: &'a [JinjaMessage],
    /// Tool definitions (OpenAI tools array). Null when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<JsonValue>,
    /// Tool choice: "auto" | "none" | "required" | specific tool name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    /// Whether to append the assistant generation prompt after messages.
    pub add_generation_prompt: bool,
    /// Enable thinking/reasoning output (for reasoning models like Qwen3).
    pub enable_thinking: bool,
    /// Whether multiple tool calls can be returned in one response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// JSON schema for structured output (from response_format or json_schema).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<String>,
    pub add_vision_id: bool,
    pub eos_token: String,
    pub bos_token: String,
}

impl<'a> TemplateContext<'a> {
    /// Build defaults from model metadata (tokens) and messages.
    pub fn new(messages: &'a [JinjaMessage], eos_token: String, bos_token: String) -> Self {
        Self {
            messages,
            eos_token,
            bos_token,

            enable_thinking: false,
            tools: None,
            tool_choice: None,
            add_generation_prompt: true,
            parallel_tool_calls: None,
            json_schema: None,
            add_vision_id: false,
        }
    }
}

pub(crate) fn create_minijinja_env() -> Environment<'static> {
    let mut env = Environment::new();

    env.set_unknown_method_callback(pycompat::unknown_method_callback);

    env.add_function(
        "raise_exception",
        |msg: String| -> Result<minijinja::Value, minijinja::Error> {
            Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                format!("Jinja Exception: {msg}"),
            ))
        },
    );

    env.add_function("strftime_now", |format: String| -> String {
        chrono::Local::now().format(&format).to_string()
    });

    env.add_function("tojson", |value: minijinja::Value| -> String {
        serde_json::to_string(&value).unwrap_or_default()
    });

    env.add_filter(
        "rsplit",
        |s: String, sep: Option<String>, maxsplit: Option<i64>| -> Vec<String> {
            let delim = sep.as_deref().unwrap_or(" ");
            match maxsplit {
                Some(n) if n > 0 => s
                    .rsplitn(n as usize + 1, delim)
                    .map(|p| p.to_string())
                    .collect(),
                _ => s.rsplit(delim).map(|p| p.to_string()).collect(),
            }
        },
    );

    env.add_filter("tojson", |value: minijinja::Value| -> String {
        serde_json::to_string(&value).unwrap_or_default()
    });

    env
}

pub(crate) fn render_chat_template(tmpl: &str, ctx: &TemplateContext) -> Result<String, String> {
    let mut env = create_minijinja_env();

    env.add_template("chat", tmpl)
        .map_err(|e| format!("can't load template: {e}"))?;

    env.get_template("chat")
        .and_then(|t| t.render(ctx))
        .map_err(|e| format!("{e}"))
}

/// Convert ToolChoiceOption enum to string for jinja template context.
pub(crate) fn tool_choice_to_string(tc: &ChatCompletionToolChoiceOption) -> String {
    match tc {
        ChatCompletionToolChoiceOption::Mode(m) => match m {
            ToolChoiceOptions::Auto => "auto".to_string(),
            ToolChoiceOptions::Required => "required".to_string(),
            ToolChoiceOptions::None => "none".to_string(),
        },
        ChatCompletionToolChoiceOption::Function(s) => s.function.name.clone(),
        _ => "auto".to_string(),
    }
}

/// Map async-openai ToolChoiceOption to LarkToolChoice for grammar controller.
pub(crate) fn to_lark_tool_choice(tc: &ChatCompletionToolChoiceOption) -> LarkToolChoice {
    match tc {
        ChatCompletionToolChoiceOption::Mode(m) => match m {
            ToolChoiceOptions::Auto => LarkToolChoice::Auto,
            ToolChoiceOptions::Required => LarkToolChoice::Required,
            ToolChoiceOptions::None => LarkToolChoice::None,
        },
        ChatCompletionToolChoiceOption::Function(_) => LarkToolChoice::Required,
        _ => LarkToolChoice::Auto,
    }
}

/// Extract (role, content_text) from a ChatCompletionRequestMessage for template rendering.
/// Mirrors the old liter_llm::Message extraction logic.
pub(crate) fn message_to_role_content(msg: &ChatCompletionRequestMessage) -> (&str, String) {
    match msg {
        ChatCompletionRequestMessage::System(m) => {
            let content = match &m.content {
                ChatCompletionRequestSystemMessageContent::Text(t) => t.clone(),
                ChatCompletionRequestSystemMessageContent::Array(_) => String::new(),
            };
            ("system", content)
        }
        ChatCompletionRequestMessage::User(m) => {
            let content = match &m.content {
                ChatCompletionRequestUserMessageContent::Text(t) => t.clone(),
                ChatCompletionRequestUserMessageContent::Array(_) => String::new(),
            };
            ("user", content)
        }
        ChatCompletionRequestMessage::Assistant(m) => {
            let content = match &m.content {
                Some(ChatCompletionRequestAssistantMessageContent::Text(t)) => t.clone(),
                Some(ChatCompletionRequestAssistantMessageContent::Array(_)) => String::new(),
                None => String::new(),
            };
            ("assistant", content)
        }
        ChatCompletionRequestMessage::Tool(m) => {
            let content = match &m.content {
                ChatCompletionRequestToolMessageContent::Text(t) => t.clone(),
                ChatCompletionRequestToolMessageContent::Array(_) => String::new(),
            };
            ("tool", content)
        }
        ChatCompletionRequestMessage::Developer(m) => {
            let content = match &m.content {
                ChatCompletionRequestDeveloperMessageContent::Text(t) => t.clone(),
                ChatCompletionRequestDeveloperMessageContent::Array(_) => String::new(),
            };
            ("developer", content)
        }
        ChatCompletionRequestMessage::Function(m) => ("function", m.name.clone()),
    }
}

/// Tokenize a prompt string using model, return error if zero tokens.
pub(crate) fn tokenize_prompt(model: &LlamaModel, prompt: &str) -> Result<Vec<Token>, String> {
    let c_prompt = CString::new(prompt).map_err(|_| "prompt contains NUL")?;
    let tokens = model.tokenize(&c_prompt, false, true);
    if tokens.is_empty() {
        return Err("prompt tokenized to zero tokens".to_string());
    }
    Ok(tokens)
}

/// Build prompt tokens for chat completion. Mirrors serve.rs build_chat_prompt_tokens
/// but takes separate parameters instead of CreateChatCompletionRequest.
pub(crate) fn build_chat_prompt_tokens(
    model: &LlamaModel,
    messages: &[ChatCompletionRequestMessage],
    tools: Option<&[ChatCompletionTools]>,
    tool_choice: Option<&ChatCompletionToolChoiceOption>,
    parallel_tool_calls: Option<bool>,
    reasoning_effort: Option<&ReasoningEffort>,
    response_format: Option<&ResponseFormat>,
) -> Result<Vec<Token>, String> {
    let tmpl = model.chat_template().ok_or("model has no chat template")?;
    let tmpl = tmpl
        .to_str()
        .map_err(|e| format!("invalid template: {e}"))?;

    let eos_token = model.eos_token_text().unwrap_or_default();
    let bos_token = model.bos_token_text().unwrap_or_default();

    // Convert Message → (role, content) → JinjaMessage
    let backing = messages
        .iter()
        .map(message_to_role_content)
        .collect::<Vec<_>>();
    let jinja_msgs: Vec<JinjaMessage> = backing
        .into_iter()
        .map(|(role, content)| JinjaMessage {
            role: role.into(),
            content,
        })
        .collect();

    let mut tmpl_ctx = TemplateContext::new(&jinja_msgs, eos_token, bos_token);

    // tools
    if let Some(tools) = tools
        && let Ok(val) = serde_json::to_value(tools)
    {
        tmpl_ctx.tools = Some(val);
    }

    // tool_choice
    if let Some(tc) = tool_choice {
        tmpl_ctx.tool_choice = Some(tool_choice_to_string(tc));
    }

    // parallel_tool_calls
    tmpl_ctx.parallel_tool_calls = parallel_tool_calls;
    // reasoning_effort → enable_thinking
    tmpl_ctx.enable_thinking = reasoning_effort.is_some();
    // response_format → json_schema
    if let Some(rf) = response_format {
        match rf {
            ResponseFormat::JsonObject => {}
            ResponseFormat::JsonSchema { json_schema } => {
                if let Some(schema) = &json_schema.schema {
                    tmpl_ctx.json_schema = Some(schema.to_string());
                }
            }
            ResponseFormat::Text => {}
        }
    }

    let prompt = render_chat_template(tmpl, &tmpl_ctx)?;

    println!("[O.o] **** jinjia ctx ****\n{tmpl_ctx:?}\n====prompt====\n{prompt}");

    tokenize_prompt(model, &prompt)
}

/// Compile tool definitions into a Lark grammar string for llguidance.
pub(crate) fn compile_tools_grammar(
    tools: &[ChatCompletionTools],
    tool_choice: LarkToolChoice,
) -> String {
    debug_assert!(!tools.is_empty());
    debug_assert!(tool_choice != LarkToolChoice::None);

    let mut rules = String::new();
    let mut tool_alternatives = Vec::new();

    for tool in tools {
        let tool = match tool {
            ChatCompletionTools::Function(t) => t,
            _ => continue,
        };
        let name = &tool.function.name;
        let params = tool
            .function
            .parameters
            .as_ref()
            .cloned()
            .unwrap_or(serde_json::json!({"type": "object", "properties": {}}));

        let wrapped = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"const": name},
                "arguments": params
            },
            "required": ["name", "arguments"]
        });
        rules.push_str(&format!("json_body_{name}: %json {wrapped}\n"));

        rules.push_str(&format!("tool_{name}_hd[lazy]: TEXT \"<tool_call>\"\n"));
        rules.push_str(&format!(
            "tool_{name}: tool_{name}_hd json_body_{name} \"</tool_call>\"\n"
        ));
        tool_alternatives.push(format!("tool_{name}"));
    }

    let tool_alternatives = tool_alternatives.join(" | ");

    const CODE_BLOCK_RULE: &str = "\
code_block: \"```\" CODE_CONTENT \"```\"\n\
CODE_CONTENT: /(.|\\n)*?/\n\
";

    match tool_choice {
        LarkToolChoice::Required => {
            format!(
                "start: (code_block | {tool_alternatives})+\n\
                 TEXT: /(.|\\n)*/\n\
                 {CODE_BLOCK_RULE}\
                 {rules}"
            )
        }
        LarkToolChoice::Auto => {
            format!(
                "start: (code_block | {tool_alternatives})* f_end\n\
                 f_end: TEXT\n\
                 TEXT: /(.|\\n)*/\n\
                 {CODE_BLOCK_RULE}\
                 {rules}"
            )
        }
        LarkToolChoice::None => {
            unreachable!();
        }
    }
}

pub(crate) fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
