use minijinja::Environment;
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct JinjaMessage {
    pub role: String,
    pub content: String,
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
    env
}
