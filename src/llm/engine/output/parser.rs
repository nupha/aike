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
    pub fn push_text(&mut self, text: &str) -> Result<(bool, OutputEvents), String> {
        let mut events: OutputEvents = smallvec![];

        // Try to detect tag boundaries
        match self.current_state {
            OutputState::Text => {
                // Append token text to buffer first, then look for open tags
                self.buf.push_str(text);

                for boundary in &self.boundaries {
                    if let Some(pos) = self.buf.find(boundary.open_tag) {
                        // Text after the open tag (starts new content)
                        self.buf = self.buf[pos + boundary.open_tag.len()..].to_string();

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
                // Multi-token close tag detection:
                // Check if text is part of the close tag before appending to buf.
                // This handles cases where the close tag spans multiple tokens
                // (e.g. Qwen3.5 generates </tool_call> as 4 BPE tokens: </ + tool + _call + >).
                //
                // Three cases:
                // 1. text is a prefix of ct (e.g. text="</", ct="</tool_call>")
                // 2. buf ends with a prefix of ct, and text continues ct
                //    (e.g. buf="...</", text="tool", ct="</tool_call>")
                // 3. buf + text ends with ct (full match, single or multi-token)
                for boundary in &self.boundaries {
                    if boundary.content_type != self.current_state {
                        continue;
                    }
                    let ct = boundary.close_tag;

                    if ct.starts_with(text) {
                        // text is part of the close tag — append to buf but don't add to content
                        self.buf.push_str(text);
                        // If text is the complete close tag (single token), trigger end event now
                        if text == ct {
                            self.buf.clear();
                            events.push(match self.current_state {
                                OutputState::Reasoning => OutputStateChange::ReasoningEnd,
                                OutputState::ToolCall => OutputStateChange::ToolCallEnd,
                                _ => unreachable!(),
                            });
                            self.current_state = OutputState::Text;
                        }
                        return Ok((false, events));
                    }

                    // Check if buf ends with a prefix of ct, and text continues ct
                    for prefix_len in (1..ct.len()).rev() {
                        let prefix = &ct[..prefix_len];
                        if self.buf.ends_with(prefix) {
                            let rest = &ct[prefix_len..];
                            if rest.starts_with(text) {
                                // text continues the close tag — append to buf but don't add to content
                                self.buf.push_str(text);
                                // If buf now contains the complete close tag, trigger end event
                                if self.buf.ends_with(ct) {
                                    self.buf.clear();
                                    events.push(match self.current_state {
                                        OutputState::Reasoning => OutputStateChange::ReasoningEnd,
                                        OutputState::ToolCall => OutputStateChange::ToolCallEnd,
                                        _ => unreachable!(),
                                    });
                                    self.current_state = OutputState::Text;
                                }
                                return Ok((false, events));
                            }
                            break;
                        }
                    }

                    // Append text to buf, then check for full close tag match
                    self.buf.push_str(text);
                    if self.buf.ends_with(ct) {
                        let content_len = self.buf.len() - ct.len();
                        self.buf = self.buf[..content_len].to_string();

                        events.push(match self.current_state {
                            OutputState::Reasoning => OutputStateChange::ReasoningEnd,
                            OutputState::ToolCall => OutputStateChange::ToolCallEnd,
                            _ => unreachable!(),
                        });
                        self.current_state = OutputState::Text;

                        return Ok((false, events));
                    }

                    // Close tag not matched — text is content
                    return Ok((true, events));
                }
            }
        }

        // No boundary detected — add text to content
        Ok((true, events))
    }

    pub fn current_state(&self) -> OutputState {
        self.current_state
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
