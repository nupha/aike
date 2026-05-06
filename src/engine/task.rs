use std::collections::{HashSet, VecDeque};

use crossbeam::channel::{Receiver, Sender};
use llama_cpp_ffi::LarkToolChoice;

use crate::helpers::now_unix_s;

use super::{
    context::Engine,
    slot::{Slot, SlotState},
    worker::remove_cancelled_inputs,
};

pub type Token = i32;

#[derive(Debug, Clone)]
pub struct GrammarConfig {
    pub(crate) lark_grammar: String,
    pub(crate) tool_choice: LarkToolChoice,
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
    pub output_mode: OutputMode,
}

impl TaskConfig {
    pub fn new() -> Self {
        Self {
            task_id: NEXT_TASK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stop_words: Vec::new(),
            max_tokens: 0,
            temperature: 1.0,
            top_p: 1.0,
            logits_for_all_tokens: false,
            grammar: None,
            output_mode: OutputMode::OneShot,
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

    pub fn output_mode(mut self, v: OutputMode) -> Self {
        self.output_mode = v;
        self
    }
}

pub struct EngineTask {
    pub config: TaskConfig,
    pub prompt_tokens: Vec<Token>,
    pub seq_hint: Option<usize>,
    pub use_cache: bool,
    pub output_tx: Sender<EngineReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Continuous,
    OneShot,
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
    Text(String),
    Stop(StopReason, usize),
    Cancelled,
    Error(String),
    Shutdown,
}

#[derive(Debug)]
pub struct TaskState {
    pub config: TaskConfig,
    pub n_completion_tokens: usize,
    pub generated_text: String,
    pub pending_bytes: Vec<u8>,
    pub output_tx: Sender<EngineReply>,
}

impl TaskState {
    pub fn append_utf8_piece(&mut self, piece: &[u8]) {
        if piece.is_empty() {
            return;
        }

        self.pending_bytes.extend_from_slice(piece);

        loop {
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(s) => {
                    self.generated_text.push_str(s);
                    self.pending_bytes.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid == 0 {
                        break;
                    }
                    let s = unsafe { std::str::from_utf8_unchecked(&self.pending_bytes[..valid]) };
                    self.generated_text.push_str(s);
                    self.pending_bytes.drain(0..valid);
                    if self.pending_bytes.is_empty() {
                        break;
                    }
                }
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

    pub(crate) fn pick_idle_slot(
        &self,
        slots: &[Slot],
        seq_hint: Option<usize>,
        prompt_tokens: &[Token],
        use_cache: bool,
        match_threshold: f32,
    ) -> Option<(usize, bool)> {
        if let Some(hint) = seq_hint
            && let Some(slot) = slots.get(hint)
            && matches!(slot.state, SlotState::Idle)
        {
            let can_reuse = use_cache && self.common_prefix_len(slot.seq_id, prompt_tokens) > 0;
            return Some((hint, can_reuse));
        }
        let idle_indices = slots
            .iter()
            .enumerate()
            .filter_map(|(idx, s)| matches!(s.state, SlotState::Idle).then_some(idx))
            .collect::<Vec<_>>();
        if idle_indices.is_empty() {
            return None;
        }

        let mut lru_idx = idle_indices[0];
        let mut lru_mtime = slots[lru_idx].mtime;
        for idx in idle_indices.iter().skip(1) {
            if slots[*idx].mtime < lru_mtime {
                lru_mtime = slots[*idx].mtime;
                lru_idx = *idx;
            }
        }

        Some((lru_idx, false))
    }

    pub(crate) fn sched_task(
        &mut self,
        task: Box<EngineTask>,
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

        let mut task = task;
        task.prompt_tokens.drain(..n_common);
        if task.prompt_tokens.is_empty() {
            let _ = task.output_tx.send(EngineReply::Error(
                "prompt_tokens became empty after prefix match".to_string(),
            ));
            return SchedResult::Rejected;
        }

        let task_state = TaskState {
            config: task.config,
            n_completion_tokens: 0,
            generated_text: String::new(),
            pending_bytes: Vec::new(),
            output_tx: task.output_tx,
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
