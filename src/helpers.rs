use minijinja::Environment;
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

pub(crate) fn preprocess_chat_template(src: &str) -> String {
    src.replace("content.startswith(", "startswith(content, ")
        .replace("content.endswith(", "endswith(content, ")
}

pub(crate) fn create_minijinja_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_function("startswith", |s: String, prefix: String| -> bool {
        s.starts_with(&prefix)
    });
    env.add_function("endswith", |s: String, suffix: String| -> bool {
        s.ends_with(&suffix)
    });

    // tojson filter: serialize value to JSON string (mirrors Jinja2's tojson)
    env.add_filter("tojson", |value: minijinja::Value| -> String {
        serde_json::to_string(&value).unwrap_or_default()
    });
    env
}

// ============================================================================
// Builtin Jinja Filters (from llama.cpp/common/jinja/value.cpp)
// ============================================================================
//
// llama.cpp implements its own Jinja2 subset engine.  Filters are registered
// per-value-type via get_builtins().  Below is the complete list extracted
// from value.cpp + runtime.cpp.
//
// -- Global functions (callable without pipe) --------------------------------
//   raise_exception(msg)         throw a Jinja exception
//   namespace(key=val, ...)      create an object from kwargs
//   strftime_now(format)         format current time
//   range([start,] stop[, step]) integer range array
//   tojson(value)                serialize to JSON string
//
// -- int filters -------------------------------------------------------------
//   | default   | abs   | float   | tojson   | string
//
// -- float filters -----------------------------------------------------------
//   | default   | abs   | int   | tojson   | string
//
// -- string filters ----------------------------------------------------------
//   | default       | upper       | lower        | strip (=trim)  | rstrip
//   | lstrip        | title       | capitalize   | length         | startswith
//   | endswith      | split       | rsplit       | replace        | int
//   | float         | string      | slice        | safe           | tojson
//   | indent(width, first, blank)               | join (not impl)
//
// -- bool filters ------------------------------------------------------------
//   | default   | int   | float   | string   | tojson
//
// -- array filters -----------------------------------------------------------
//   | default   | list      | first     | last      | length
//   | slice     | select    | selectattr | reject   | rejectattr
//   | join      | string    | tojson     | map      | append
//   | pop       | sort      | reverse    | unique (not impl)
//
// -- object filters ----------------------------------------------------------
//   | get(key[, default])  | keys      | values    | items
//   | tojson               | string    | length    | dictsort
//   | join (not impl)
//
// -- none filters ------------------------------------------------------------
//   | default   | tojson   | string   | safe      | strip
//   | items / map / reject / rejectattr / select / selectattr / unique  -> empty array
//
// -- undefined filters -------------------------------------------------------
//   | default       | capitalize / lower / upper / title / strip / replace
//   | safe / string / truncate / wordcount          -> empty string
//   | first / last / max / min                      -> undefined
//   | length / sum                                  -> 0
//   | items / list / map / reject / rejectattr / select / selectattr
//   | sort / reverse / unique                       -> empty array
//
// -- test expressions ("value is <test>") ------------------------------------
//   boolean   callable   odd   even   false   true   divisibleby
//   string    integer    float   number   iterable   sequence
//   mapping   lower   upper   none   defined   undefined
//   eq / equalto   ge   gt / greaterthan   lt / lessthan   ne
//   in   sameas (not impl)   escaped (not impl)   filter (not impl)
//
// Note: "trim" is an alias for "strip" (handled in runtime.cpp)
// ============================================================================
