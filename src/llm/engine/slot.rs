use std::collections::VecDeque;

use super::{
    Engine,
    task::{EngineReply, TaskState, Token},
};

#[derive(Debug)]
pub enum SlotState {
    Idle,
    Decoding {
        prompt_tokens: VecDeque<Token>,
        logits_for_all_tokens: bool,
        num_written: usize,
        task_state: TaskState,
    },
    Sampling {
        logits: i32,
        task_state: TaskState,
    },
    Done {
        task_state: TaskState,
        reply: EngineReply,
    },
    Busy,
}

pub struct Slot {
    pub seq_id: usize,
    pub n_past: i32,
    pub mtime: i64,
    pub state: SlotState,
}

impl Slot {
    pub fn new(seq_id: usize) -> Self {
        Self {
            seq_id,
            n_past: 0,
            mtime: 0,
            state: SlotState::Idle,
        }
    }
}

impl Engine {
    fn common_prefix_len(&self, seq_id: usize, prompt_tokens: &[Token]) -> usize {
        let cached: &Vec<Token> = match self.cached_tokens().get(seq_id) {
            Some(c) => c,
            None => return 0,
        };
        let max_common = cached.len().min(prompt_tokens.len());
        let mut n = 0usize;
        while n < max_common && cached[n] == prompt_tokens[n] {
            n += 1;
        }
        n
    }

    /// Align KV/cache state for prefix reuse. Returns reusable prefix length.
    pub(super) fn prepare_slot(
        &mut self,
        seq_id: usize,
        prompt_tokens: &[Token],
    ) -> Result<usize, String> {
        if seq_id >= self.cached_tokens().len() {
            return Err(format!("invalid seq id {}", seq_id));
        }

        let mut n_common: usize = {
            let cached = self.cached_tokens();
            let slot_cache = &cached[seq_id];
            let max_common = slot_cache.len().min(prompt_tokens.len());
            let mut cnt = 0_usize;

            while cnt < max_common && slot_cache[cnt] == prompt_tokens[cnt] {
                cnt += 1;
            }
            if cnt == prompt_tokens.len() && cnt > 0 {
                cnt -= 1;
            }

            cnt
        };

        let llm = self.llm_ctx_mut();
        let removed = llm.memory_seq_rm(seq_id as i32, n_common as i32, -1);
        if !removed {
            llm.memory_seq_rm(seq_id as i32, 0, -1);
            n_common = 0;
        }

        self.cached_tokens_mut()[seq_id].truncate(n_common);

        Ok(n_common)
    }

    pub(crate) fn truncate_slot_cache(&mut self, seq_id: usize, n_keep: usize) {
        let cached: &mut Vec<Vec<Token>> = self.cached_tokens_mut();
        if let Some(c) = cached.get_mut(seq_id) {
            c.truncate(n_keep);
        }
    }

    pub(super) fn pick_idle_slot(
        &self,
        slots: &[Slot],
        seq_hint: Option<usize>,
        prompt_tokens: &[Token],
        use_cache: bool,
        _match_threshold: f32,
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

    /// reset state for given slot id
    pub(crate) fn reset_slot(&mut self, seq_id: usize) {
        let ss = self.samplers_mut();
        if seq_id < ss.len() {
            ss[seq_id] = None;
        }
    }

    pub(crate) fn clear_slot(&mut self, seq_id: usize) {
        if seq_id < self.cached_tokens().len() {
            self.llm_ctx_mut().memory_seq_rm(seq_id as i32, 0, -1);
            self.cached_tokens_mut()[seq_id].clear();
            self.samplers_mut()[seq_id] = None;
        }
    }
}
