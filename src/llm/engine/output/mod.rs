mod parser;

pub use parser::{OutputParser, TagBoundary};

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
