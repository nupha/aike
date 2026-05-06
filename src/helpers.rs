use std::ffi::CString;

use liter_llm::{
    ChatCompletionTool, ReasoningEffort,
    types::common::{
        ContentPart, Message, ResponseFormat, ToolChoice, ToolChoiceMode, UserContent,
    },
};
use llama_cpp_ffi::{LarkToolChoice, LlamaModel};
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

    // Use pycompat callback for dot method syntax (obj.method()).
    // Handles 25+ Python methods: str.startswith/endswith/find/strip/split/..., dict.get/items/..., list.count
    env.set_unknown_method_callback(pycompat::unknown_method_callback);

    // -- Global functions (mirrors llama.cpp jinja/global_builtins) --
    // Note: range() and namespace() are already provided by minijinja builtins.

    // raise_exception(msg): throw a Jinja exception (llama.cpp: throws raised_exception)
    env.add_function(
        "raise_exception",
        |msg: String| -> Result<minijinja::Value, minijinja::Error> {
            Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                format!("Jinja Exception: {msg}"),
            ))
        },
    );

    // strftime_now(format): format current local time (llama.cpp: uses C strftime)
    env.add_function("strftime_now", |format: String| -> String {
        chrono::Local::now().format(&format).to_string()
    });

    // tojson as global function
    env.add_function("tojson", |value: minijinja::Value| -> String {
        serde_json::to_string(&value).unwrap_or_default()
    });

    // -- Filters NOT covered by pycompat or minijinja builtins --
    // pycompat handles dot methods: startswith/endswith/strip/lstrip/rstrip/split/replace/...
    // minijinja builtins provide pipe filters: trim/upper/lower/title/capitalize/replace/length/...

    // rsplit: split from right (not in pycompat)
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

    // tojson filter (override built-in to match llama.cpp serialization)
    env.add_filter("tojson", |value: minijinja::Value| -> String {
        serde_json::to_string(&value).unwrap_or_default()
    });

    // append: append item to array
    env.add_filter(
        "append",
        |arr: minijinja::Value, item: minijinja::Value| -> minijinja::Value {
            let mut items: Vec<minijinja::Value> = Vec::new();
            if let Ok(iter) = arr.try_iter() {
                for v in iter {
                    items.push(v);
                }
            }
            items.push(item);
            minijinja::Value::from_serialize(&items)
        },
    );

    // pop: get element from array by index (default: last)
    env.add_filter(
        "pop",
        |arr: minijinja::Value, index: Option<i64>| -> minijinja::Value {
            let len = arr.len().unwrap_or(0);
            if len == 0 {
                return minijinja::Value::from(());
            }
            let idx = index.unwrap_or(-1);
            let real = if idx < 0 { len as i64 + idx } else { idx };
            if real < 0 || real as usize >= len {
                minijinja::Value::from(())
            } else {
                arr.get_item_by_index(real as usize)
                    .unwrap_or_else(|_| minijinja::Value::from(()))
            }
        },
    );

    // get: get value from object by key with optional default
    env.add_filter(
        "get",
        |obj: minijinja::Value,
         key: String,
         default: Option<minijinja::Value>|
         -> minijinja::Value {
            obj.get_attr(&key)
                .ok()
                .filter(|v| !v.is_undefined())
                .unwrap_or_else(|| default.unwrap_or_else(|| minijinja::Value::from(())))
        },
    );

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

/// Convert ToolChoice enum to string for jinja template context.
pub(crate) fn tool_choice_to_string(tc: &ToolChoice) -> String {
    match tc {
        ToolChoice::Mode(mode) => match mode {
            ToolChoiceMode::Auto => "auto",
            ToolChoiceMode::Required => "required",
            ToolChoiceMode::None => "none",
        }
        .to_string(),
        ToolChoice::Specific(s) => s.function.name.clone(),
    }
}

/// Map liter_llm ToolChoice to LarkToolChoice for grammar controller.
pub(crate) fn to_lark_tool_choice(tc: &ToolChoice) -> LarkToolChoice {
    match tc {
        ToolChoice::Mode(mode) => match mode {
            ToolChoiceMode::Auto => LarkToolChoice::Auto,
            ToolChoiceMode::Required => LarkToolChoice::Required,
            ToolChoiceMode::None => LarkToolChoice::None,
        },
        ToolChoice::Specific(_) => LarkToolChoice::Required,
    }
}

/// Extract (role, content_text) from a liter_llm::Message for template rendering.
pub(crate) fn message_to_role_content(msg: &Message) -> (&str, String) {
    let role = match msg {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::Tool(_) => "tool",
        Message::Developer(_) => "developer",
        Message::Function(_) => "function",
    };
    let content = match msg {
        Message::System(m) => m.content.clone(),
        Message::User(m) => match &m.content {
            UserContent::Text(s) => s.clone(),
            UserContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        },
        Message::Assistant(m) => m.content.clone().unwrap_or_default(),
        Message::Tool(m) => m.content.clone(),
        Message::Developer(m) => m.content.clone(),
        Message::Function(m) => m.content.clone(),
    };
    (role, content)
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
/// but takes separate parameters instead of ChatCompletionRequest.
pub(crate) fn build_chat_prompt_tokens(
    model: &LlamaModel,
    messages: &[Message],
    tools: Option<&[ChatCompletionTool]>,
    tool_choice: Option<&ToolChoice>,
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
    if let Some(effort) = reasoning_effort {
        tmpl_ctx.enable_thinking = !matches!(effort, ReasoningEffort::Low);
    }

    // response_format → json_schema
    if let Some(rf) = response_format {
        match rf {
            ResponseFormat::JsonObject => {}
            ResponseFormat::JsonSchema { json_schema } => {
                tmpl_ctx.json_schema = Some(json_schema.schema.to_string());
            }
            ResponseFormat::Text => {}
        }
    }

    let prompt = render_chat_template(tmpl, &tmpl_ctx)?;
    tokenize_prompt(model, &prompt)
}

/// Compile tool definitions into a Lark grammar string for llguidance.
///
/// The grammar constrains the model output to valid JSON matching the tool schemas.
/// Each tool's parameters are embedded as `%json {schema}` directives.
/// The grammar allows free text interleaved with tool calls.
///
/// The generated JSON follows the standard Qwen/Hermes tool call JSON format:
///   {"name": "function_name", "arguments": {"city": "Shanghai"}}
/// where `arguments` is a JSON object constrained by the params schema.
/// The parser will later serialize `arguments` to a JSON string for the OpenAI format.
pub(crate) fn compile_tools_lark_grammar(
    tools: &[ChatCompletionTool],
    tool_choice: LarkToolChoice,
) -> String {
    debug_assert!(!tools.is_empty());
    debug_assert!(tool_choice != LarkToolChoice::None);

    let mut rules = String::new();
    let mut tool_alternatives = Vec::new();

    for tool in tools {
        let name = &tool.function.name;
        let params = tool
            .function
            .parameters
            .as_ref()
            .cloned()
            .unwrap_or(serde_json::json!({"type": "object", "properties": {}}));

        // Wrap the original parameters schema inside a {"name": const, "arguments": {...}} envelope.
        // This matches the standard Qwen/Hermes tool call JSON format:
        //   {"name": "function_name", "arguments": {...}}
        let wrapped = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"const": name},
                "arguments": params
            },
            "required": ["name", "arguments"]
        });
        rules.push_str(&format!("json_body_{name}: %json {wrapped}\n"));

        // Use lazy TEXT so that when the model emits "<tool_call>",
        // TEXT stops matching and the parser switches to the tool path.
        rules.push_str(&format!("tool_{name}_hd[lazy]: TEXT \"<tool_call>\"\n"));
        rules.push_str(&format!(
            "tool_{name}: tool_{name}_hd json_body_{name} \"</tool_call>\"\n"
        ));
        tool_alternatives.push(format!("tool_{name}"));
    }

    let tool_alternatives = tool_alternatives.join(" | ");

    match tool_choice {
        LarkToolChoice::Required => {
            // force tool call, no free text
            format!(
                "start: ({tool_alternatives})+\n\
                 TEXT: /(.|\\n)*/\n\
                 {rules}"
            )
        }
        LarkToolChoice::Auto | LarkToolChoice::None => {
            // allow free text or tool calls, interleaved.
            // Uses lazy TEXT so that when the model emits "<tool_call>",
            // TEXT stops matching and the parser switches to tool_xxx.
            format!(
                "start: ({tool_alternatives})* f_end\n\
                 f_end: TEXT\n\
                 TEXT: /(.|\\n)*/\n\
                 {rules}"
            )
        }
    }
}

pub(crate) fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
