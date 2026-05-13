use std::fmt;

/// Represents the semantic state of LLM output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    /// Normal text generation
    Text,
    /// Reasoning/thinking block
    Reasoning,
    /// Tool call block
    ToolCall,
}

impl fmt::Display for OutputState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputState::Text => write!(f, "text"),
            OutputState::Reasoning => write!(f, "reasoning"),
            OutputState::ToolCall => write!(f, "tool_call"),
        }
    }
}
