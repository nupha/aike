mod parser;

pub use parser::OutputParser;

/// Callback type for validating whether a given name is a single token in vocab or not.
pub type LlmTokenValidator = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// Represents the semantic state of LLM output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    Text,
    Reasoning,
    ToolCall,
}

/// Events when chunk boundaries are detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStateChange {
    ReasoningStart,
    ReasoningEnd,
    ToolCallStart,
    ToolCallEnd,
}
