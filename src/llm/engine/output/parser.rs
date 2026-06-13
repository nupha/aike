use smallvec::{SmallVec, smallvec};

use super::{OutputAction, OutputState, OutputStateChange};

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
/// Uses a simple state machine with string matching on accumulated text.
/// Detects open/close tags and emits state change events accordingly.
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
    /// contains an open tag (e.g. from chat template) so the parser
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
    /// Returns `(action, events)`:
    /// - `action`: what the caller should do (add content, hold, or react to state change)
    /// - `events`: state change events detected (0-2 per token)
    pub fn add_text(&mut self, text: &str) -> Result<(OutputAction, OutputEvents), String> {
        let mut events: OutputEvents = smallvec![];

        match self.current_state {
            OutputState::Text => {
                // Append token text to buffer, then apply three-state tag detection:
                // 1. buf == open_tag        → complete tag  → TagStart
                // 2. buf is prefix of open_tag → partial tag → Hold
                // 3. buf is not any prefix   → normal text  → Content(buf)
                self.buf.push_str(text);

                // Check against all open tags
                for boundary in &self.boundaries {
                    let ot = boundary.open_tag;

                    // Complete match: buf contains the full open tag
                    if let Some(pos) = self.buf.find(ot) {
                        // Content before the tag is safe to emit
                        let before = self.buf[..pos].to_string();
                        self.buf.clear();

                        events.push(match boundary.content_type {
                            OutputState::Reasoning => OutputStateChange::ReasoningStart,
                            OutputState::ToolCall => OutputStateChange::ToolCallStart,
                            _ => unreachable!(),
                        });
                        self.current_state = boundary.content_type;

                        return if before.is_empty() {
                            Ok((OutputAction::TagStart, events))
                        } else {
                            // Emit preceding content; caller will see TagStart via events
                            // We can't return two actions, so carry content and signal via events
                            Ok((OutputAction::Content(before), events))
                        };
                    }

                    // Prefix check: is buf a proper prefix of this open tag?
                    if ot.starts_with(&self.buf) && self.buf.len() < ot.len() {
                        // buf is a partial tag prefix — hold
                        return Ok((OutputAction::Hold, events));
                    }
                }

                // No complete tag, not a valid prefix — emit buf as normal content
                let content = std::mem::take(&mut self.buf);
                Ok((OutputAction::Content(content), events))
            }
            OutputState::Reasoning | OutputState::ToolCall => {
                // Multi-token close tag detection:
                // Check if text is part of the close tag before appending to buf.
                // This handles cases where the close tag spans multiple tokens
                // (e.g. Qwen3.5 generates close tag as 4 BPE tokens).
                //
                // Three cases:
                // 1. text is a prefix of ct (e.g. text="</", ct="")
                // 2. buf ends with a prefix of ct, and text continues ct
                //    (e.g. buf="...</", text="tool", ct="")
                // 3. buf + text ends with ct (full match, single or multi-token)
                for boundary in &self.boundaries {
                    if boundary.content_type != self.current_state {
                        continue;
                    }
                    let ct = boundary.close_tag;

                    if ct.starts_with(text) {
                        // text is part of the close tag — append to buf, don't add to content
                        self.buf.push_str(text);
                        // If text is the complete close tag (single token), trigger end event
                        if text == ct {
                            self.buf.clear();
                            events.push(match self.current_state {
                                OutputState::Reasoning => OutputStateChange::ReasoningEnd,
                                OutputState::ToolCall => OutputStateChange::ToolCallEnd,
                                _ => unreachable!(),
                            });
                            self.current_state = OutputState::Text;
                            return Ok((OutputAction::TagEnd, events));
                        }
                        return Ok((OutputAction::Hold, events));
                    }

                    // Check if buf ends with a prefix of ct, and text continues ct
                    for prefix_len in (1..ct.len()).rev() {
                        let prefix = &ct[..prefix_len];
                        if self.buf.ends_with(prefix) {
                            let rest = &ct[prefix_len..];
                            if rest.starts_with(text) {
                                // text continues the close tag — append to buf
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
                                    return Ok((OutputAction::TagEnd, events));
                                }
                                return Ok((OutputAction::Hold, events));
                            }
                            break;
                        }
                    }

                    // Append text to buf, then check for full close tag match
                    self.buf.push_str(text);
                    if self.buf.ends_with(ct) {
                        // Content before the close tag is safe to emit
                        let before_len = self.buf.len() - ct.len();
                        let before = self.buf[..before_len].to_string();
                        self.buf.clear();
                        events.push(match self.current_state {
                            OutputState::Reasoning => OutputStateChange::ReasoningEnd,
                            OutputState::ToolCall => OutputStateChange::ToolCallEnd,
                            _ => unreachable!(),
                        });
                        self.current_state = OutputState::Text;
                        return if before.is_empty() {
                            Ok((OutputAction::TagEnd, events))
                        } else {
                            Ok((OutputAction::Content(before), events))
                        };
                    }

                    // Close tag not matched — text is content
                    let content = std::mem::take(&mut self.buf);
                    return Ok((OutputAction::Content(content), events));
                }
                // No matching boundary for current state — emit text directly
                let content = std::mem::take(&mut self.buf);
                let mut result = content;
                result.push_str(text);
                Ok((OutputAction::Content(result), events))
            }
        }
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
