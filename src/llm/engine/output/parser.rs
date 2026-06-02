use llama_cpp_ffi::{LlamaModel, llama_token};
use smallvec::{SmallVec, smallvec};
use swiftlet_incremental::{IncrementalParser, LarkTokenValidation, Swiftlet};

use super::{LlmTokenValidator, OutputState, OutputStateChange};

/// Type alias for small event vector (optimized for 0-2 events per token)
pub type OutputEvents = SmallVec<[OutputStateChange; 2]>;

/// Builder for OutputParser with fluent API.
pub struct OutputParserBuilder {
    model: Option<LlamaModel>,
    grammar_str: Option<String>,
    token_validator: Option<LlmTokenValidator>,
}

impl OutputParserBuilder {
    pub fn new() -> Self {
        Self {
            model: None,
            grammar_str: None,
            token_validator: None,
        }
    }

    pub fn model(mut self, model: LlamaModel) -> Self {
        self.model = Some(model);
        self
    }

    pub fn grammar(mut self, grammar: &str) -> Self {
        self.grammar_str = Some(grammar.to_string());
        self
    }

    pub fn token_validator<F>(mut self, validator: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.token_validator = Some(Box::new(validator));
        self
    }

    pub fn build(self) -> Result<OutputParser, String> {
        let model = self
            .model
            .ok_or("Model is required. Use .model() to set it.")?;
        let grammar_str = self
            .grammar_str
            .ok_or("Grammar string is required. Use .grammar() to set it.")?;

        OutputParser::new_with_builder(model, grammar_str.as_str(), self.token_validator)
    }
}

/// Output parser that tracks semantic state of LLM token streams.
pub struct OutputParser {
    current_state: OutputState,
    parser: swiftlet_incremental::IncrementalParser,
    model: LlamaModel,
}

impl OutputParser {
    fn new_with_builder(
        model: LlamaModel,
        grammar_str: &str,
        token_validator: Option<LlmTokenValidator>,
    ) -> Result<Self, String> {
        let swiftlet = Swiftlet::from_str(grammar_str)
            .map_err(|e| format!("Failed to parse output grammar: {}", e))?;

        let parser = match token_validator {
            Some(cb) => IncrementalParser::with_token_validator(
                swiftlet.frontend(),
                "start".to_string(),
                Some(Box::new(move |terminal: &str| {
                    if terminal.starts_with("TOK_") && cb(terminal) {
                        LarkTokenValidation::ValidAlways
                    } else {
                        LarkTokenValidation::Invalid
                    }
                })),
            ),
            None => IncrementalParser::new(swiftlet.frontend(), "start".to_string()),
        };

        Ok(Self {
            current_state: OutputState::Text,
            parser,
            model,
        })
    }

    pub fn builder() -> OutputParserBuilder {
        OutputParserBuilder::new()
    }

    pub fn push_token(&mut self, llm_token: llama_token) -> Result<(bool, OutputEvents), String> {
        let mut events: OutputEvents = smallvec![];
        let mut text = String::new();
        let mut add_text = false;

        // Save raw token text before terminal mapping — needed for fail-guard fallback.
        // For TOK_* terminals, `text` stays empty because output_token_to_terminal
        // returns Some(...), so the unwrap_or_else closure that sets `text` is skipped.
        let raw_token_text = self.model.lookup_token_text(llm_token).map(|s| s.to_string());

        let terminal = if let Some(t) = raw_token_text.as_deref() {
            self.model
                .arch()
                .output_token_to_terminal(t)
                .unwrap_or_else(|| {
                    text = t.to_string();
                    add_text = true;
                    "TEXT"
                })
        } else {
            add_text = true;
            "TEXT"
        };

        // Push token to parser for grammar validation
        if let Err(e) = self.parser.push_token_str(&text, terminal) {
            eprintln!(
                "[O.o][OutputParser::push_token] token={llm_token}, terminal={terminal:?}, text={text}, err={e}",
            );

            // FAIL GUARD: If the parser rejects a TOK_* terminal, try to push it as TEXT instead.
            // This handles cases where the model outputs a special token in an unexpected
            // context (e.g., stray boundary token outside its expected section).
            if terminal != "TEXT" {
                // Use raw_token_text for fallback: `text` is empty for TOK_* terminals
                // because it's only set in the TEXT fallback path above.
                let fallback = raw_token_text.as_deref().unwrap_or(&text);
                if self.parser.push_token_str(fallback, "TEXT").is_ok() {
                    // Accepted as plain text — keep current state, add text
                    add_text = true;
                    return Ok((add_text, events));
                }
            }

            self.reset();
            return Err(format!("Output error: {e}, token={llm_token}, text={text}"));
        }

        let mut new_state: Option<OutputState> = None;

        // Check if the terminal is chunk boundary (TOK_* terminals)
        if terminal != "TEXT"
            && let Some((ctype, is_start)) = self.model.arch().output_check_boundary(terminal)
        {
            // Convert from llama_cpp_ffi Boundary to local OutputState (identical enum)
            let ctype = match ctype {
                llama_cpp_ffi::ContentType::Text => OutputState::Text,
                llama_cpp_ffi::ContentType::Reasoning => OutputState::Reasoning,
                llama_cpp_ffi::ContentType::ToolCall => OutputState::ToolCall,
            };

            if is_start {
                match ctype {
                    OutputState::Reasoning => {
                        events.push(OutputStateChange::ReasoningStart);
                        new_state = Some(OutputState::Reasoning);
                    }
                    OutputState::ToolCall => {
                        events.push(OutputStateChange::ToolCallStart);
                        new_state = Some(OutputState::ToolCall);
                    }
                    _ => {}
                }
            } else {
                new_state = Some(OutputState::Text);
                match ctype {
                    OutputState::Reasoning => {
                        events.push(OutputStateChange::ReasoningEnd);
                    }
                    OutputState::ToolCall => {
                        events.push(OutputStateChange::ToolCallEnd);
                    }
                    _ => {}
                }
            }
        }

        if let Some(new_state) = new_state {
            self.current_state = new_state;
        }

        Ok((add_text, events))
    }

    pub fn current_state(&self) -> OutputState {
        self.current_state
    }

    pub fn reset(&mut self) {
        self.parser.reset();
        self.current_state = OutputState::Text;
    }
}

impl std::fmt::Debug for OutputParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputParser")
            .field("current_state", &self.current_state)
            .field("parser", &"<swiftlet::IncrementalParser>")
            .field("position", &self.parser.position())
            .finish()
    }
}
