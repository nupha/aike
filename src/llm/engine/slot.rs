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

    pub(super) fn pick_idle_slot(
        &self,
        slots: &[Slot],
        seq_hint: Option<usize>,
        prompt_tokens: &[Token],
        match_threshold: f32,
    ) -> Option<(usize, usize)> {
        // seq_hint takes priority: caller suggests which slot to reuse
        if let Some(hint) = seq_hint
            && let Some(slot) = slots.get(hint)
            && matches!(slot.state, SlotState::Idle)
        {
            let n = self.common_prefix_len(slot.seq_id, prompt_tokens);
            return Some((hint, n));
        }

        // Scan idle slots: track best match (longest common prefix above threshold)
        // and LRU fallback simultaneously.
        let mut best_idx = None;
        let mut best_n = 0usize;
        let mut lru_idx = None;
        let mut lru_n = 0usize;
        let mut lru_mtime = i64::MAX;

        for (idx, slot) in slots.iter().enumerate() {
            if !matches!(slot.state, SlotState::Idle) {
                continue;
            }

            let n = self.common_prefix_len(slot.seq_id, prompt_tokens);

            // Track best match above threshold
            if n > best_n
                && (match_threshold <= 0.0
                    || n as f32 / prompt_tokens.len() as f32 >= match_threshold)
            {
                best_n = n;
                best_idx = Some(idx);
            }

            // Track LRU as fallback
            if slot.mtime < lru_mtime {
                lru_mtime = slot.mtime;
                lru_n = n;
                lru_idx = Some(idx);
            }
        }

        // Prefer best match; fallback to LRU
        best_idx
            .or(lru_idx)
            .map(|idx| (idx, if Some(idx) == best_idx { best_n } else { lru_n }))
    }

    /// Align KV/cache state for prefix reuse. Returns reusable prefix length.
    /// `n_common` is the raw common prefix length (from `pick_idle_slot`); this
    /// function adjusts it so that at least one token remains for the model to
    /// process.
    pub(super) fn prepare_slot(
        &mut self,
        seq_id: usize,
        prompt_tokens: &[Token],
        n_common: usize,
    ) -> Result<usize, String> {
        if seq_id >= self.cached_tokens().len() {
            return Err(format!("invalid seq id {}", seq_id));
        }

        // When the prompt fully matches the cached prefix, keep one token so
        // the model still has input to forward and can generate the next token.
        // Without this, draining all tokens would leave prompt_tokens empty and
        // the prefill stage would have nothing to process.
        let n_common = if n_common == prompt_tokens.len() && n_common > 0 {
            n_common - 1
        } else {
            n_common
        };

        if n_common == 0 {
            self.clear_slot(seq_id);
            Ok(0)
        } else {
            let llm = self.llm_ctx_mut();
            // For hybrid models (e.g. Qwen3.5), the cache has both attention
            // and recurrent layers.  The recurrent state does not support
            // arbitrary partial truncation — it can only roll back within
            // n_rs_seq steps (default 0).  When memory_seq_rm fails, we
            // retry with a truncated range bounded by n_rs_seq.  If that
            // also fails, fall back to clearing the entire slot.
            let removed = llm.memory_seq_rm(seq_id as i32, n_common as i32, -1);
            if !removed {
                let n_rs = llm.n_rs_seq() as usize;
                if n_rs > 0 {
                    let n_clip = n_common.min(n_rs);
                    log::debug!(
                        "[prepare_slot] seq_rm failed for seq_id={}, \
                         retrying with n_rs_seq={} clip (n_common={} -> n_clip={})",
                        seq_id,
                        n_rs,
                        n_common,
                        n_clip,
                    );
                    if llm.memory_seq_rm(seq_id as i32, n_clip as i32, -1) {
                        self.cached_tokens_mut()[seq_id].truncate(n_clip);
                        return Ok(n_clip);
                    }
                }

                log::debug!(
                    "[prepare_slot] seq_rm failed (hybrid model? n_rs_seq={n_rs} too small?), \
                     falling back to full clear for seq_id={seq_id}"
                );

                self.clear_slot(seq_id);
                Ok(0)
            } else {
                self.cached_tokens_mut()[seq_id].truncate(n_common);
                Ok(n_common)
            }
        }
    }

    pub(crate) fn truncate_slot_cache(&mut self, seq_id: usize, n_keep: usize) {
        let cached: &mut Vec<Vec<Token>> = self.cached_tokens_mut();
        if let Some(c) = cached.get_mut(seq_id) {
            c.truncate(n_keep);
        }
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
