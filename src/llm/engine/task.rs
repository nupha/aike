use std::{
    collections::{HashSet, VecDeque},
    io::Write,
    time::Duration,
};

/// Maximum time a task can wait in the deferred queue before being rejected as Busy.
const TASK_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

use async_openai::types::chat::ChatCompletionTools;
use crossbeam::channel::{Receiver, Sender};
use llama_cpp_ffi::{ContentType, LlamaModel, SamplerChain, ToolChoice};

use crate::helpers::now_unix_s;

use super::cache::CHECKPOINT_STEP;

use super::{
    context::Engine,
    output::{
        GeneratedContent, OutputAction, OutputParser, OutputState, OutputStateChange, TagBoundary,
    },
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

        let tool_iter = tools.iter().filter_map(|t| match t {
            ChatCompletionTools::Function(f) => {
                let name = f.function.name.clone();
                let params = f
                    .function
                    .parameters
                    .clone()
                    .unwrap_or(serde_json::json!({"type": "object", "properties": {}}));
                Some((name, params))
            }
            _ => None,
        });

        Self {
            lark_grammar: SamplerChain::build_tools_grammar(
                tool_iter,
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
    /// Number of initial prompt tokens to keep during context shift.
    /// When KV cache is full, tokens [keep_tokens..keep_tokens+n_discard] are
    /// removed and the rest are shifted forward. 0 = keep nothing (full clear).
    /// Typically set to the system prompt token count.
    pub keep_tokens: usize,
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
            keep_tokens: 0,
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

    pub fn keep_tokens(mut self, v: usize) -> Self {
        self.keep_tokens = v;
        self
    }
}

pub struct EngineTask {
    pub config: TaskConfig,

    pub prompt_tokens: Vec<Token>,

    /// Prepended tokens as output
    pub output_prepend: Vec<Token>,
    pub output_tx: Sender<EngineReply>,

    /// Set to Some(Instant) when the task is deferred due to all slots busy.
    /// If the wait exceeds TASK_WAIT_TIMEOUT, the task is rejected with Busy.
    pub timestamp: Option<std::time::Instant>,
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
    /// Pending bytes for UTF-8 decoding across token boundaries.
    /// Both parser and content accumulation share this single buffer:
    /// bytes are extended per-token, valid UTF-8 prefixes are consumed,
    /// incomplete trailing sequences carry over to the next token.
    pub(crate) pending_bytes: Vec<u8>,
    /// Token output log (debugging). Each generated token text is appended here.
    #[cfg(debug_assertions)]
    pub(crate) token_log: Option<std::fs::File>,
}

impl TaskState {
    pub(super) fn add_output_token(&mut self, token: Token, model: &LlamaModel) {
        self.n_completion_tokens += 1;

        // Log every generated token for debugging (debug builds only)
        #[cfg(debug_assertions)]
        if let Some(ref mut log) = self.token_log {
            let piece = model.token_to_piece(token);
            let text = String::from_utf8_lossy(&piece);
            let _ = writeln!(log, "[tok={token}] {text}");
        }

        // Buffer raw bytes and extract valid UTF-8 prefix.
        // Incomplete bytes stay in pending_bytes for the next token to complete.
        let piece = model.token_to_piece(token);
        self.pending_bytes.extend_from_slice(&piece);
        let decoded = decode_utf8_buf(&mut self.pending_bytes);
        let Some(ref text) = decoded else {
            // No valid UTF-8 yet — bytes stay in pending_bytes,
            // will be decoded when the next token completes the sequence.
            return;
        };

        if let Some(ref mut parser) = self.output_parser {
            match parser.add_text(text) {
                Ok((action, event_list)) => {
                    // Append content BEFORE processing events: events may swap
                    // the content block (e.g. TagStart creates a new ToolCall block),
                    // so any preceding text must land in the current block first.
                    if let OutputAction::Content(ref s) = action {
                        if !s.is_empty() {
                            self.current_content.last_mut().unwrap().push_str(s);
                        }
                    }

                    for event in event_list {
                        self.handle_output_event(event);
                    }
                }
                Err(_) => {
                    // Parser reset internally: clear pending bytes (they belong to
                    // pre-error state and are no longer valid), then sync content
                    let parser_state = parser.current_state();

                    self.pending_bytes.clear();

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
        } else {
            // No parser: append decoded text directly to content
            debug_assert!(!self.current_content.is_empty());
            self.current_content.last_mut().unwrap().push_str(text);
        }
    }

    fn handle_output_event(&mut self, event: OutputStateChange) {
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
}

/// Extract a valid UTF-8 prefix from a byte buffer, draining consumed bytes.
/// Returns `Some(text)` if at least one complete character is available,
/// or `None` if the buffer contains only an incomplete UTF-8 sequence.
fn decode_utf8_buf(buf: &mut Vec<u8>) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    if let Ok(s) = std::str::from_utf8(buf) {
        let text = s.to_string();
        buf.clear();
        Some(text)
    } else if let Err(e) = std::str::from_utf8(buf) {
        let valid = e.valid_up_to();
        if valid > 0 {
            let s = unsafe { std::str::from_utf8_unchecked(&buf[..valid]) };
            let text = s.to_string();
            buf.drain(0..valid);
            Some(text)
        } else {
            None
        }
    } else {
        None
    }
}

pub enum SchedResult {
    Scheduled,
    Deferred(Box<EngineTask>),
    Busy(Box<EngineTask>),
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
                            self.clear_slot(slot);
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
                                is_prefill,
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
                                        is_prefill,
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
        debug_assert!(!task.prompt_tokens.is_empty());
        // if task.prompt_tokens.is_empty() {
        //     let _ = task.output_tx.send(EngineReply::Error(
        //         "prompt_tokens became empty after prefix match".to_string(),
        //     ));
        //     return SchedResult::Rejected;
        // }

        let Some((slot_idx, mut n_common_raw)) =
            self.pick_idle_slot(slots, &task.prompt_tokens, prefix_match_threshold)
        else {
            // No idle slot found
            let now = std::time::Instant::now();
            return if now.duration_since(*task.timestamp.get_or_insert(now)) > TASK_WAIT_TIMEOUT {
                SchedResult::Busy(task)
            } else {
                SchedResult::Deferred(task)
            };
        };

        log::debug!(
            "[sched] task#{} prompt_tokens.len={} first10={:?}",
            task.config.task_id,
            task.prompt_tokens.len(),
            &task.prompt_tokens[..task.prompt_tokens.len().min(10)],
        );

        // Full match from slot prefix: recurrent/hybrid models can't safely
        // truncate KV cache, so treat as no-match and fall through to cache_ram.
        if n_common_raw == task.prompt_tokens.len()
            && self.context_mode() == super::cache::ContextMode::Checkpoint
        {
            n_common_raw = 0;
        }

        if n_common_raw == 0 {
            // try match in cache ram
            log::debug!(
                "[sched] no slot match, searching cache_ram ({} entries) for prompt.len={}",
                self.cache_ram.entry_count(),
                task.prompt_tokens.len(),
            );

            if let Some((cache_idx, len)) = self
                .cache_ram
                .find_best_match(&task.prompt_tokens, self.context_mode())
            {
                log::debug!(
                    "[sched] cache_ram hit: entry#{} match_len={}",
                    cache_idx,
                    len,
                );
                let r = self.restore_cache_ram_to_slot(&mut slots[slot_idx], cache_idx, len);
                debug_assert!(r > 0, "fail to restore cache_ram entry");
                n_common_raw = len;
            } else {
                log::debug!("[sched] cache_ram miss: no matching entry found");
            }
        }

        // Full-match handling after all prefix (slot + cache_ram) resolution.
        // When the entire prompt matches cached state with no new input tokens,
        // the model won't generate any output.  cached_tokens is a flat
        // [prompt|generated] sequence without a prompt/generation boundary,
        // so we cannot reliably extract just the generated portion.
        if n_common_raw == task.prompt_tokens.len() {
            if self.context_mode() == super::cache::ContextMode::Checkpoint {
                // Recurrent: can't safely trim KV to force re-generation.
                // Signal empty generation. Slot stays Idle with cache intact.
                log::debug!(
                    "[sched] slot#{slot_idx} prompt fully cached (n_common_raw={}), \
                     recurrent model, no new tokens to process",
                    n_common_raw,
                );
                let _ = task
                    .output_tx
                    .send(EngineReply::Stop(StopReason::PrefillOnly, 0));
                return SchedResult::Scheduled;
            } else {
                // Non-recurrent: trim one token from the common prefix so at
                // least one prompt token is re-decoded, forcing the model to
                // generate new output beyond the cached prefix.
                n_common_raw = n_common_raw.saturating_sub(1);
            }
        }

        let n_common = match self.prefill_slot(
            &mut slots[slot_idx],
            &task.prompt_tokens,
            n_common_raw,
            self.n_cache_reuse,
        ) {
            Ok(n) => n,
            Err(e) => {
                let _ = task.output_tx.send(EngineReply::Error(e));
                return SchedResult::Rejected;
            }
        };

        log::debug!(
            "[sched] slot#{slot_idx} seq_id:{} prompt:{} common:{n_common_raw} -> {n_common}",
            slots[slot_idx].seq_id,
            slots[slot_idx].cached_tokens.len(),
        );

        if n_common > 0 {
            // trim common tokens from prompt
            task.prompt_tokens.drain(..n_common);
        }

        // Create output parser if enabled
        let output_parser: Option<OutputParser> = if task.config.enable_output_parser {
            let boundaries = self
                .model
                .arch()
                .tag_boundaries()
                .into_iter()
                .map(|(ct, open, close)| {
                    let content_type = match ct {
                        ContentType::Reasoning => OutputState::Reasoning,
                        ContentType::ToolCall => OutputState::ToolCall,
                        _ => OutputState::Text,
                    };
                    TagBoundary {
                        content_type,
                        open_tag: open,
                        close_tag: close,
                    }
                })
                .collect::<Vec<TagBoundary>>();

            // If the prompt already contains an unclosed open tag (e.g. `<think>`
            // from chat template), start the parser in that state directly.
            // The caller signals this by populating `output_prepend`.
            let initial_state = if !task.output_prepend.is_empty() {
                // Decode prepended tokens to text, then check which open tag they contain
                let mut prepend_bytes = Vec::new();
                for t in &task.output_prepend {
                    prepend_bytes.extend_from_slice(&self.model.token_to_piece(*t));
                }
                let prepend_text = String::from_utf8_lossy(&prepend_bytes);
                let mut state = OutputState::Text;
                for b in &boundaries {
                    if prepend_text.contains(b.open_tag) {
                        state = b.content_type;
                        break;
                    }
                }
                state
            } else {
                OutputState::Text
            };

            Some(OutputParser::new(boundaries, initial_state))
        } else {
            None
        };

        // Add initial current_content element.
        // If the parser starts in a non-Text state (e.g. Reasoning because the
        // prompt already contains an unclosed <think> tag), emit the corresponding
        // Start event so downstream handlers see it.
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
            let log_path = format!("aike-output-tokens.log");
            // Create/truncate the log file
            std::fs::File::create(&log_path).ok()
        } else {
            None
        };

        let mut task_state = TaskState {
            config: task.config,
            n_completion_tokens: 0,
            output_parser,
            output_tx: task.output_tx,
            current_content,
            pending_bytes: Vec::new(),
            #[cfg(debug_assertions)]
            token_log,
        };

        // If the parser starts in a non-Text state (e.g. Reasoning because the
        // prompt already contains an unclosed <think> tag), emit the corresponding
        // Start event so downstream handlers see it.
        if initial_state != OutputState::Text {
            let event = match initial_state {
                OutputState::Reasoning => OutputStateChange::ReasoningStart,
                OutputState::ToolCall => OutputStateChange::ToolCallStart,
                _ => unreachable!(),
            };
            task_state.handle_output_event(event);
        }

        let slot = &mut slots[slot_idx];
        slot.n_past = n_common as i32;
        slot.mtime = now_unix_s();

        // Set aligned dump target for Checkpoint mode:
        // dump_at = largest CHECKPOINT_STEP multiple <= total_prompt_len
        // This dumps once during prefill, excluding the generation prompt suffix.
        if self.context_mode() == super::cache::ContextMode::Checkpoint {
            let total_prompt_len = n_common + task.prompt_tokens.len();
            let dump_at = (total_prompt_len / CHECKPOINT_STEP) * CHECKPOINT_STEP;
            slot.checkpoint_dump_at = if dump_at > n_common { dump_at } else { 0 };
        }

        slot.state = SlotState::Decoding {
            prompt_tokens: VecDeque::from(task.prompt_tokens),
            logits_for_all_tokens: task_state.config.logits_for_all_tokens,
            num_written: 0,
            task_state,
            is_prefill: true,
        };

        SchedResult::Scheduled
    }
}
