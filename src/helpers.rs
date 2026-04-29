use minijinja::Environment;
use minijinja_contrib::pycompat;
use serde::Serialize;
use serde_json::Value as JsonValue;

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
