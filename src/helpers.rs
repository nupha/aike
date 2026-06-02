use async_openai::types::chat::{
    ChatCompletionRequestAssistantMessageContent, ChatCompletionRequestDeveloperMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessageContent,
    ChatCompletionToolChoiceOption, ChatCompletionTools, CreateChatCompletionRequest,
    ReasoningEffort, ResponseFormat, ToolChoiceOptions,
};
use crossbeam::channel::TryRecvError;
use llama_cpp_ffi::{LlamaModel, ToolChoice as LarkToolChoice};
use minijinja::Environment;
use minijinja_contrib::pycompat;
use serde::Serialize;
use serde_json::Value as JsonValue;
use zuqe::{JobState, ScopedContext};

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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let total_secs = now.as_secs();
        let ((year, month, day), (hours, minutes, seconds)) = secs_to_datetime(total_secs);

        format
            .replace("%Y", &format!("{:04}", year))
            .replace("%m", &format!("{:02}", month))
            .replace("%d", &format!("{:02}", day))
            .replace("%H", &format!("{:02}", hours))
            .replace("%M", &format!("{:02}", minutes))
            .replace("%S", &format!("{:02}", seconds))
            .replace("%s", &total_secs.to_string())
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

pub(crate) fn apply_chat_template(
    model: &LlamaModel,
    messages: &[ChatCompletionRequestMessage],
    tools: Option<&[ChatCompletionTools]>,
    tool_choice: Option<&ChatCompletionToolChoiceOption>,
    parallel_tool_calls: Option<bool>,
    reasoning_effort: ReasoningEffort,
    response_format: Option<&ResponseFormat>,
) -> Result<String, String> {
    let eos_token = model.eos_token_text().unwrap_or_default();
    let bos_token = model.bos_token_text().unwrap_or_default();

    model
        .with_chat_template(|tmpl| {
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
            tmpl_ctx.enable_thinking = reasoning_effort != ReasoningEffort::None;
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

            render_chat_template(tmpl, &tmpl_ctx)
        })
        .ok_or("model has no chat template".to_string())?
}

/// Result of scanning from the end of a prompt for think tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptThinkPattern {
    Open,
    Close,
    No,
}

/// For given prompt, scan from the tail backwards and return the first think related tag encountered.
pub(crate) fn detect_prompt_think_pattern(
    prompt: &str,
    open_tag: &str,  // for example, "<think>"
    close_tag: &str, // for example, "</think>"
) -> PromptThinkPattern {
    let s = prompt.trim_end();

    let close_pos = s.rfind(close_tag);
    let open_pos = s.rfind(open_tag);

    match (close_pos, open_pos) {
        (Some(cp), Some(op)) if cp > op => PromptThinkPattern::Close,
        (Some(_), Some(_)) => PromptThinkPattern::Open,
        (Some(_), None) => PromptThinkPattern::Close,
        (None, Some(_)) => PromptThinkPattern::Open,
        (None, None) => PromptThinkPattern::No,
    }
}

/// Convert a camelCase string to snake_case.
fn camel_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.char_indices() {
        if i > 0 && ch.is_uppercase() {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

/// Recursively convert all object keys in a JSON value from camelCase to snake_case.
pub(crate) fn json_keys_camel_to_snake(val: &mut JsonValue) {
    match val {
        JsonValue::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let snake = camel_to_snake(&key);
                if snake != key
                    && let Some(v) = map.remove(&key)
                {
                    map.insert(snake, v);
                }
            }
            // recurse into values
            for v in map.values_mut() {
                json_keys_camel_to_snake(v);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                json_keys_camel_to_snake(v);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_chat_completion_req(
    mut json_val: serde_json::Value,
) -> Result<CreateChatCompletionRequest, serde_json::Error> {
    // Convert camelCase JS keys to snake_case for serde deserialization
    json_keys_camel_to_snake(&mut json_val);

    // Note: model is required, set default if missing
    if let Some(obj) = json_val.as_object_mut()
        && !obj.contains_key("model")
    {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String("".to_string()),
        );
    }

    serde_json::from_value(json_val)
}

pub(crate) fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format current UTC time as RFC 3339 (e.g. "2025-05-31T12:00:00Z")
pub(crate) fn format_timestamp_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let (ymd, hms) = secs_to_datetime(total_secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        ymd.0, ymd.1, ymd.2, hms.0, hms.1, hms.2
    )
}

/// Format current UTC time as compact string (e.g. "20250531_120000")
pub(crate) fn format_timestamp_compact() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let (ymd, hms) = secs_to_datetime(total_secs);
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        ymd.0, ymd.1, ymd.2, hms.0, hms.1, hms.2
    )
}

/// Convert seconds since epoch to (year, month, day) and (hours, minutes, seconds) in UTC.
fn secs_to_datetime(total_secs: u64) -> ((i64, u32, u32), (u32, u32, u32)) {
    let days = total_secs / 86400;
    let time_secs = total_secs % 86400;
    let hours = (time_secs / 3600) as u32;
    let minutes = ((time_secs % 3600) / 60) as u32;
    let seconds = (time_secs % 60) as u32;

    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0u32;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining < md {
            m = i as u32;
            break;
        }
        remaining -= md;
    }
    let day = (remaining + 1) as u32;
    ((y, m + 1, day), (hours, minutes, seconds))
}

/// Start a persistent polling microtask driven by a crossbeam channel.
///
/// The poller calls `handler(msg, cx)` for each received message. If the
/// handler returns `true` the poller continues (`Again`); `false` triggers
/// a call to `on_finish()` and terminates (`Done`).
///
/// When the channel is empty the poller checks `has_entries()`. If there
/// are still live entries it polls again; otherwise it calls `on_finish()`
/// and terminates.  A disconnected channel always triggers termination.
pub(crate) fn start_channel_poller<T, F>(
    ctx: &mut ScopedContext,
    rx: crossbeam::channel::Receiver<T>,
    handler: F,
    has_entries: impl Fn() -> bool + 'static,
    on_finish: impl Fn() + 'static,
) where
    T: 'static,
    F: Fn(T, &mut ScopedContext) -> bool + 'static,
{
    ctx.queue_job(
        move |cx| match rx.try_recv() {
            Ok(msg) => {
                if handler(msg, cx) {
                    Ok(JobState::Again)
                } else {
                    on_finish();
                    Ok(JobState::Done)
                }
            }
            Err(TryRecvError::Empty) => {
                if has_entries() {
                    Ok(JobState::Again)
                } else {
                    on_finish();
                    Ok(JobState::Done)
                }
            }
            Err(TryRecvError::Disconnected) => {
                on_finish();
                Ok(JobState::Done)
            }
        },
        (),
    );
}
