use smallvec::{SmallVec, smallvec};

use super::{OutputState, OutputStateChange};

/// Type alias for small event vector (optimized for 0-2 events per token)
pub type OutputEvents = SmallVec<[OutputStateChange; 2]>;

/// A tag boundary definition: when the accumulated text contains `open_tag`,
/// we enter `content_type` state; when it contains `close_tag`, we exit back to Text.
#[derive(Debug, Clone)]
pub struct TagBoundary {
    pub content_type: OutputState,
    pub open_tag: &'static str,
    pub close_tag: &'static str,
}

/// A lightweight stream parser that detects tag boundaries in LLM token output.
///
/// Instead of using a full Lark grammar + Earley parser (swiftlet-incremental),
/// this parser uses a simple state machine with string matching on accumulated text.
/// It detects open/close tags (e.g. `<tool_call>`/`</tool_call>`, `<think>`/`</think>`)
/// and emits state change events accordingly.
///
/// This approach avoids the multi-path tokenization problem: since detection is
/// done at the text level (after UTF-8 decoding), it doesn't matter whether a tag
/// is generated as a single special token or multiple BPE tokens.
pub struct OutputParser {
    current_state: OutputState,
    /// Accumulated text buffer for tag detection
    buf: String,
    /// Tag boundary definitions
    boundaries: Vec<TagBoundary>,
}

impl OutputParser {
    /// Create a new parser with the given tag boundaries.
    ///
    /// `boundaries` defines the tag pairs to detect. Order matters: if multiple
    /// open tags could match at the same position, the first one wins.
    ///
    /// `initial_state` sets the starting state. Use this when the prompt already
    /// contains an open tag (e.g. `<think>` from chat template) so the parser
    /// starts in the correct state without needing to replay prepended tokens.
    pub fn new(boundaries: Vec<TagBoundary>, initial_state: OutputState) -> Self {
        Self {
            current_state: initial_state,
            buf: String::new(),
            boundaries,
        }
    }

    /// Push a token's decoded text into the parser.
    ///
    /// Returns `(add_text, events)`:
    /// - `add_text`: whether the caller should append this token's text to content
    /// - `events`: state change events detected (0-2 per token)
    pub fn push_token(&mut self, text: &str) -> Result<(bool, OutputEvents), String> {
        let mut events: OutputEvents = smallvec![];

        // Append token text to buffer
        self.buf.push_str(text);

        // Try to detect tag boundaries
        match self.current_state {
            OutputState::Text => {
                // Look for open tags
                for boundary in &self.boundaries {
                    if let Some(pos) = self.buf.find(boundary.open_tag) {
                        // Text before the open tag
                        let before = self.buf[..pos].to_string();
                        // Text after the open tag (starts new content)
                        self.buf = self.buf[pos + boundary.open_tag.len()..].to_string();

                        // If there was text before the tag, it stays as Text content
                        // (the caller already appended it via add_text=true from previous tokens)
                        // We don't need to emit it as an event — it's already in current_content

                        events.push(match boundary.content_type {
                            OutputState::Reasoning => OutputStateChange::ReasoningStart,
                            OutputState::ToolCall => OutputStateChange::ToolCallStart,
                            _ => unreachable!(),
                        });
                        self.current_state = boundary.content_type;

                        // Don't add the tag text to content
                        return Ok((false, events));
                    }
                }
            }
            OutputState::Reasoning | OutputState::ToolCall => {
                // Look for close tag matching current state
                for boundary in &self.boundaries {
                    if boundary.content_type != self.current_state {
                        continue;
                    }
                    if self.buf.ends_with(boundary.close_tag) {
                        // Strip the close tag from buffer
                        let content_len = self.buf.len() - boundary.close_tag.len();
                        let content = self.buf[..content_len].to_string();
                        self.buf = content;

                        events.push(match self.current_state {
                            OutputState::Reasoning => OutputStateChange::ReasoningEnd,
                            OutputState::ToolCall => OutputStateChange::ToolCallEnd,
                            _ => unreachable!(),
                        });
                        self.current_state = OutputState::Text;

                        // Don't add the close tag text to content
                        return Ok((false, events));
                    }
                }
            }
        }

        // No boundary detected — add text to content
        Ok((true, events))
    }

    pub fn current_state(&self) -> OutputState {
        self.current_state
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.current_state = OutputState::Text;
    }
}

impl std::fmt::Debug for OutputParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputParser")
            .field("current_state", &self.current_state)
            .field("buf_len", &self.buf.len())
            .finish()
    }
}
