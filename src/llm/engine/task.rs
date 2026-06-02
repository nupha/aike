use std::collections::{HashSet, VecDeque};
use std::io::Write;

use async_openai::types::chat::ChatCompletionTools;
use crossbeam::channel::{Receiver, Sender};
use llama_cpp_ffi::{LlamaModel, ToolChoice};

use crate::helpers::{build_tools_grammar, now_unix_s};

use super::{
    context::Engine,
    output::{LlmTokenValidator, OutputParser, OutputState, OutputStateChange},
    slot::{Slot, SlotState},
    worker::remove_cancelled_inputs,
};

pub type Token = i32;

#[derive(Debug, Clone)]
pub struct GrammarConfig {
    pub(crate) lark_grammar: String,
    pub(crate) tool_choice: ToolChoice,
}

impl GrammarConfig {
    pub fn new(model: &LlamaModel, tools: &[ChatCompletionTools], tool_choice: ToolChoice) -> Self {
        debug_assert!(!tools.is_empty());

        Self {
            lark_grammar: build_tools_grammar(
                tools,
                tool_choice,
                model.arch().tool_call_boundaries(),
            ),
            tool_choice,
        }
    }
}

static NEXT_TASK_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

#[derive(Debug, Clone)]
pub struct TaskConfig {
    pub task_id: usize,
    pub grammar: Option<GrammarConfig>,
    pub stop_words: Vec<String>,
    pub max_tokens: usize,
    pub temperature: f32,
    pub top_p: f32,
    pub logits_for_all_tokens: bool,
    pub output_stream: bool,
    /// Enable output parsing to track semantic states (reasoning, tool calls, etc.)
    pub enable_output_parser: bool,
}

impl TaskConfig {
    pub fn new() -> Self {
        Self {
            task_id: NEXT_TASK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stop_words: Vec::new(),
            max_tokens: 0,
            temperature: 0.75,
            top_p: 1.0,
            logits_for_all_tokens: false,
            grammar: None,
            output_stream: false,
            enable_output_parser: false,
        }
    }

    pub fn stop_words(mut self, v: Vec<String>) -> Self {
        self.stop_words = v;
        self
    }

    pub fn max_tokens(mut self, v: usize) -> Self {
        self.max_tokens = v;
        self
    }

    pub fn temperature(mut self, v: f32) -> Self {
        self.temperature = v;
        self
    }

    pub fn top_p(mut self, v: f32) -> Self {
        self.top_p = v;
        self
    }

    pub fn logits_for_all_tokens(mut self, v: bool) -> Self {
        self.logits_for_all_tokens = v;
        self
    }

    pub fn grammar(mut self, g: GrammarConfig) -> Self {
        self.grammar = Some(g);
        self
    }

    pub fn output_stream(mut self, is_stream: bool) -> Self {
        self.output_stream = is_stream;
        self
    }

    pub fn enable_output_parser(mut self, enable: bool) -> Self {
        self.enable_output_parser = enable;
        self
    }
}

pub struct EngineTask {
    pub config: TaskConfig,

    pub prompt_tokens: Vec<Token>,

    /// Prepended tokens as output
    pub output_prepend: Vec<Token>,
    pub output_tx: Sender<EngineReply>,

    pub seq_hint: Option<usize>,
    pub use_cache: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCtrlMsg {
    Shutdown,
    CancelTask(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Eog,
    MaxTokens,
    PrefillOnly,
    Word(String),
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

/// Helper function to check if a GeneratedContent matches the given state
fn matches_content(content: &GeneratedContent, state: &OutputState) -> bool {
    matches!(
        (content, state),
        (GeneratedContent::Text(_), OutputState::Text)
            | (GeneratedContent::Reasoning(_), OutputState::Reasoning)
            | (GeneratedContent::ToolCall(_), OutputState::ToolCall)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineReply {
    Content(GeneratedContent),
    Stop(StopReason, usize),
    Cancelled,
    Error(String),
    Shutdown,
}

#[derive(Debug)]
pub struct TaskState {
    pub(crate) config: TaskConfig,
    pub(crate) n_completion_tokens: usize,
    pub(crate) output_parser: Option<OutputParser>,
    pub(crate) output_tx: Sender<EngineReply>,

    /// Accumulated generated contents.
    /// In OneShot mode: collects all contents, sent at the end.
    /// In Continuous mode: sent immediately on state transition.
    /// The last element represents the current content being accumulated.
    pub(crate) current_content: Vec<GeneratedContent>,
    /// Pending bytes for UTF-8 decoding
    pub(crate) pending_bytes: Vec<u8>,
    /// Token output log (debugging). Each generated token text is appended here.
    #[cfg(debug_assertions)]
    pub(crate) token_log: Option<std::fs::File>,
}

impl TaskState {
    pub(crate) fn add_output_token(&mut self, token: Token, model: &LlamaModel) {
        let mut append_token_utf8 = true;
        self.n_completion_tokens += 1;

        // Log every generated token for debugging (debug builds only)
        #[cfg(debug_assertions)]
        if let Some(ref mut log) = self.token_log {
            let piece = model.token_to_piece(token);
            let text = String::from_utf8_lossy(&piece);
            let _ = writeln!(log, "[tok={token}] {text}");
        }

        // Push token to parser and get events
        if let Some(ref mut parser) = self.output_parser {
            match parser.push_token(token) {
                Ok((add_text, event_list)) => {
                    for event in event_list {
                        self.handle_output_event(event);
                    }
                    append_token_utf8 = add_text;
                }
                Err(e) => {
                    append_token_utf8 = false;

                    #[cfg(debug_assertions)]
                    eprintln!("[O.o][TaskState::add_output_token] Error: {e}, token={token}");

                    // Sync current_content state with parser after error
                    // (parser may have been reset internally)
                    let parser_state = parser.current_state();
                    let needs_sync = {
                        let last = self.current_content.last().unwrap();
                        !matches!(
                            (last, parser_state),
                            (GeneratedContent::Text(_), OutputState::Text)
                                | (GeneratedContent::Reasoning(_), OutputState::Reasoning)
                                | (GeneratedContent::ToolCall(_), OutputState::ToolCall)
                        )
                    };
                    if needs_sync {
                        let new_content = match parser_state {
                            OutputState::Text => GeneratedContent::Text(String::new()),
                            OutputState::Reasoning => GeneratedContent::Reasoning(String::new()),
                            OutputState::ToolCall => GeneratedContent::ToolCall(String::new()),
                        };
                        let last_content = self.current_content.last_mut().unwrap();
                        let old = std::mem::replace(last_content, new_content);
                        if self.config.output_stream && !old.as_str().is_empty() {
                            let _ = self.output_tx.send(EngineReply::Content(old));
                        }
                    }
                }
            }
        }

        if append_token_utf8 {
            self.append_utf8_bytes(&model.token_to_piece(token));
        }
    }

    fn handle_output_event(&mut self, event: OutputStateChange) {
        #[cfg(debug_assertions)]
        eprintln!("[O.o] TaskState::handle_output_event({event:?})");

        // At chunk boundary (state change) and in stream mode: send current content
        if self.config.output_stream {
            for content in self.current_content.drain(..) {
                if !content.as_str().is_empty() {
                    let _ = self.output_tx.send(EngineReply::Content(content));
                }
            }
            self.pending_bytes.clear();
        }

        // Push new empty content for the new state
        self.current_content.push(match event {
            OutputStateChange::ReasoningStart => GeneratedContent::Reasoning(String::new()),
            OutputStateChange::ToolCallStart => GeneratedContent::ToolCall(String::new()),
            _ => GeneratedContent::Text(String::new()),
        });
    }

    fn append_utf8_bytes(&mut self, piece: &[u8]) {
        debug_assert!(!piece.is_empty());
        debug_assert!(!self.current_content.is_empty());

        #[cfg(debug_assertions)]
        {
            // Get current state from parser
            let state = self
                .output_parser
                .as_ref()
                .map(|p| p.current_state())
                .unwrap_or(OutputState::Text);

            // Debug assert: parser state should match last content element's type
            debug_assert!(
                matches_content(self.current_content.last().unwrap(), &state),
                "Parser state mismatch with current_content"
            );
        }

        self.pending_bytes.extend_from_slice(piece);

        while !self.pending_bytes.is_empty() {
            let text: String = match std::str::from_utf8(&self.pending_bytes) {
                Ok(s) => {
                    let text = s.to_string();
                    self.pending_bytes.clear();
                    text
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let s = unsafe {
                            std::str::from_utf8_unchecked(&self.pending_bytes[..valid]).to_string()
                        };
                        self.pending_bytes.drain(0..valid);
                        s
                    } else if self.pending_bytes.len() >= 4 {
                        self.pending_bytes.drain(0..1);
                        continue;
                    } else {
                        String::new()
                    }
                }
            };

            if !text.is_empty() {
                self.current_content.last_mut().unwrap().push_str(&text);
            }
        }
    }
}

pub enum SchedResult {
    Scheduled,
    Deferred(Box<EngineTask>),
    Rejected,
}

impl Engine {
    pub(crate) fn handle_ctrl_messages(
        &mut self,
        ctrl_rx: &Receiver<EngineCtrlMsg>,
        slots: &mut [Slot],
        ready: &mut VecDeque<Box<EngineTask>>,
        deferred: &mut VecDeque<Box<EngineTask>>,
        cancelled_ids: &mut HashSet<usize>,
    ) -> bool {
        while let Ok(ctrl) = ctrl_rx.try_recv() {
            match ctrl {
                EngineCtrlMsg::Shutdown => {
                    for slot in slots.iter_mut() {
                        let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

                        if let SlotState::Decoding { task_state, .. }
                        | SlotState::Sampling { task_state, .. }
                        | SlotState::Done { task_state, .. } = state0
                        {
                            let _ = task_state.output_tx.send(EngineReply::Shutdown);
                            self.clear_slot(slot.seq_id);
                        }
                        slot.n_past = 0;
                        slot.state = SlotState::Idle;
                    }
                    return true;
                }
                EngineCtrlMsg::CancelTask(task_id) => {
                    remove_cancelled_inputs(ready, task_id);
                    remove_cancelled_inputs(deferred, task_id);
                    cancelled_ids.insert(task_id);

                    for slot in slots.iter_mut() {
                        let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

                        slot.state = match state0 {
                            SlotState::Decoding {
                                prompt_tokens,
                                logits_for_all_tokens,
                                num_written,
                                task_state,
                            } => {
                                if task_state.config.task_id == task_id {
                                    SlotState::Done {
                                        task_state,
                                        reply: EngineReply::Cancelled,
                                    }
                                } else {
                                    SlotState::Decoding {
                                        prompt_tokens,
                                        logits_for_all_tokens,
                                        num_written,
                                        task_state,
                                    }
                                }
                            }
                            SlotState::Sampling { logits, task_state } => {
                                if task_state.config.task_id == task_id {
                                    SlotState::Done {
                                        task_state,
                                        reply: EngineReply::Cancelled,
                                    }
                                } else {
                                    SlotState::Sampling { logits, task_state }
                                }
                            }
                            SlotState::Done { task_state, reply } => {
                                if task_state.config.task_id == task_id {
                                    SlotState::Done {
                                        task_state,
                                        reply: EngineReply::Cancelled,
                                    }
                                } else {
                                    SlotState::Done { task_state, reply }
                                }
                            }
                            other => other,
                        };
                    }
                }
            }
        }

        false
    }

    pub(crate) fn sched_task(
        &mut self,
        mut task: Box<EngineTask>,
        slots: &mut [Slot],
        prefix_match_threshold: f32,
    ) -> SchedResult {
        if task.prompt_tokens.is_empty() {
            let _ = task.output_tx.send(EngineReply::Error(
                "prompt_tokens must not be empty".to_string(),
            ));
            return SchedResult::Rejected;
        }
        let Some((slot_idx, allow_cache_reuse)) = self.pick_idle_slot(
            slots,
            task.seq_hint,
            &task.prompt_tokens,
            task.use_cache,
            prefix_match_threshold,
        ) else {
            return SchedResult::Deferred(task);
        };

        let seq_id = slots[slot_idx].seq_id;
        let n_common = if allow_cache_reuse {
            match self.prepare_slot(seq_id, &task.prompt_tokens) {
                Ok(n) => n,
                Err(e) => {
                    let _ = task.output_tx.send(EngineReply::Error(e));
                    return SchedResult::Rejected;
                }
            }
        } else {
            self.clear_slot(seq_id);
            self.truncate_slot_cache(seq_id, 0);
            0
        };

        task.prompt_tokens.drain(..n_common);
        if task.prompt_tokens.is_empty() {
            let _ = task.output_tx.send(EngineReply::Error(
                "prompt_tokens became empty after prefix match".to_string(),
            ));
            return SchedResult::Rejected;
        }

        // Create output parser if enabled
        let output_parser: Option<OutputParser> = if task.config.enable_output_parser {
            let grammar = self.model.arch().output_grammar();
            let token_validator: LlmTokenValidator = Box::new({
                let model2 = self.model.clone();
                move |token_text: &str| -> bool { model2.lookup_text_token(token_text).is_some() }
            });

            let mut p = OutputParser::builder()
                .model(self.model.clone())
                .grammar(&grammar)
                .token_validator(token_validator)
                .build()
                .ok();

            if !task.output_prepend.is_empty()
                && let Some(p) = &mut p
            {
                // mimic model generates an open think tag, to make parser be prepared with proper state
                for token in task.output_prepend {
                    let _ = p.push_token(token);
                }
            }

            p
        } else {
            None
        };

        // Add initial current_content element
        let initial_state = output_parser
            .as_ref()
            .map(|p| p.current_state())
            .unwrap_or(OutputState::Text);

        let current_content = vec![match initial_state {
            OutputState::Text => GeneratedContent::Text(String::new()),
            OutputState::Reasoning => GeneratedContent::Reasoning(String::new()),
            OutputState::ToolCall => GeneratedContent::ToolCall(String::new()),
        }];

        // Debug token log: cleared at start of each inference
        #[cfg(debug_assertions)]
        let token_log = if task.config.enable_output_parser {
            let log_path = format!("/tmp/aike-tokens-{}.log", task.config.task_id);
            // Create/truncate the log file
            std::fs::File::create(&log_path).ok()
        } else {
            None
        };

        let task_state = TaskState {
            config: task.config,
            n_completion_tokens: 0,
            output_parser,
            output_tx: task.output_tx,
            current_content,
            pending_bytes: Vec::new(),
            #[cfg(debug_assertions)]
            token_log,
        };

        let slot = &mut slots[slot_idx];
        slot.n_past = n_common as i32;
        slot.mtime = now_unix_s();
        slot.state = SlotState::Decoding {
            prompt_tokens: VecDeque::from(task.prompt_tokens),
            logits_for_all_tokens: task_state.config.logits_for_all_tokens,
            num_written: 0,
            task_state,
        };

        SchedResult::Scheduled
    }
}
