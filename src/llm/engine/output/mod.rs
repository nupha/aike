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

/// Action returned by OutputParser.add_text(), tells the caller what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputAction {
    /// Append the carried text to current content block.
    /// Usually just the token text; may include buffered prefix when
    /// a held-back partial tag turns out not to be a real tag.
    Content(String),
    /// Text is buffered as a potential tag prefix, no action needed.
    Hold,
    /// Open tag detected, state transitioned (e.g. Text → ToolCall).
    TagStart,
    /// Close tag detected, state transitioned (e.g. ToolCall → Text).
    TagEnd,
}

/// Represents different types of generated content based on semantic state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratedContent {
    /// Normal text output
    Text(String),
    /// Reasoning/thinking content
    Reasoning(String),
    /// Tool call content (typically JSON)
    ToolCall(String),
}

impl GeneratedContent {
    /// Get the text content regardless of type
    pub fn as_str(&self) -> &str {
        match self {
            GeneratedContent::Text(s) => s,
            GeneratedContent::Reasoning(s) => s,
            GeneratedContent::ToolCall(s) => s,
        }
    }

    /// Append string to content
    pub fn push_str(&mut self, s: &str) {
        match self {
            GeneratedContent::Text(text) => text.push_str(s),
            GeneratedContent::Reasoning(text) => text.push_str(s),
            GeneratedContent::ToolCall(text) => text.push_str(s),
        }
    }

    /// Truncate content to new length
    pub fn truncate(&mut self, new_len: usize) {
        match self {
            GeneratedContent::Text(text) => text.truncate(new_len),
            GeneratedContent::Reasoning(text) => text.truncate(new_len),
            GeneratedContent::ToolCall(text) => text.truncate(new_len),
        }
    }
}
