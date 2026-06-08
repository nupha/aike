use std::collections::VecDeque;

use llama_cpp_ffi::SamplerChain;

use super::{
    Engine,
    cache::ContextMode,
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
        /// true = prefill phase (processing prompt tokens),
        /// false = sample->decode phase (generating one token at a time).
        is_prefill: bool,
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
    /// Cached tokens for this slot, used for prefix matching and
    /// for dumping/restoring KV cache state to/from CacheRam.
    pub cached_tokens: Vec<Token>,
    /// last tokens count when dumped to cache_ram (Checkpoint mode).
    pub last_dump_tokens: usize,
    /// Aligned dump target for current task (Checkpoint mode).
    /// Set once in sched_task based on total prompt length.
    /// 0 = no dump scheduled.
    pub checkpoint_dump_at: usize,
    /// Sampler chain for this slot, lazily initialized.
    pub sampler: Option<SamplerChain>,
}

impl Slot {
    pub fn new(seq_id: usize) -> Self {
        Self {
            seq_id,
            n_past: 0,
            mtime: 0,
            state: SlotState::Idle,
            cached_tokens: Vec::new(),
            last_dump_tokens: 0,
            checkpoint_dump_at: 0,
            sampler: None,
        }
    }
}

impl Engine {
    pub(super) fn pick_idle_slot(
        &mut self,
        slots: &mut [Slot],
        prompt_tokens: &[Token],
        match_threshold: f32,
    ) -> Option<(usize, usize)> {
        // Scan idle slots: track best match (longest common prefix above threshold)
        // and LRU fallback simultaneously.
        let is_checkpoint = self.context_mode() == ContextMode::Checkpoint;
        let mut best_idx = None;
        let mut best_n = 0usize;
        let mut lru_idx = None;
        let mut lru_mtime = i64::MAX;

        for (idx, slot) in slots
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s.state, SlotState::Idle))
        {
            let n = common_prefix_len(&slot.cached_tokens, prompt_tokens);

            // In Checkpoint mode, only full match (n == cached_tokens) is valid.
            // Partial matches can't truncate, so treat them as no match.
            let valid = if is_checkpoint {
                n == slot.cached_tokens.len()
            } else {
                match_threshold <= 0.0 || n as f32 / prompt_tokens.len() as f32 >= match_threshold
            };

            if n > best_n && valid {
                best_n = n;
                best_idx = Some(idx);
            }

            // Track LRU as fallback
            if slot.mtime < lru_mtime {
                lru_mtime = slot.mtime;
                lru_idx = Some(idx);
            }
        }

        if let Some(best) = best_idx {
            log::debug!("reuse best slot #{best}, n_common={best_n}");
            Some((best, best_n))
        } else if let Some(lru) = lru_idx {
            log::debug!("pick lru slot #{lru}");
            Some((lru, 0))
        } else {
            None
        }
    }

    /// Align KV/cache state for prefix reuse. Returns reusable prefix length.
    ///
    /// When `n_cache_reuse > 0` and KV shifting is supported, after the initial
    /// prefix match, scans the remaining cached tokens for additional matching
    /// chunks and uses `memory_seq_rm` + `memory_seq_add` to shift them into
    /// the correct position, avoiding recomputation of the common prefix.
    ///
    /// `n_common` is the raw common prefix length (from `pick_idle_slot`); this
    /// function adjusts it so that at least one token remains for the model to
    /// process.
    /// `n_cache_reuse` is the minimum chunk size to attempt shifting (0 = disabled).
    ///
    /// Returns the total number of reusable tokens (n_past).
    pub(super) fn prefill_slot(
        &mut self,
        slot: &mut Slot,
        prompt_tokens: &[Token],
        n_common: usize,
        n_cache_reuse: usize,
    ) -> Result<usize, String> {
        debug_assert!(!prompt_tokens.is_empty());
        debug_assert!(n_common < prompt_tokens.len());

        #[cfg(debug_assertions)]
        if self.context_mode() == ContextMode::Checkpoint {
            debug_assert!(
                n_common == 0 || n_common == slot.cached_tokens.len(),
                "n_common={n_common}, cached_tokens={}",
                slot.cached_tokens.len()
            );
        } else {
            debug_assert!(n_common <= slot.cached_tokens.len());
        }

        if n_common == 0 {
            self.clear_slot(slot);
            return Ok(0);
        }

        let seq_id = slot.seq_id;
        let can_shift = self.llm_ctx().memory_can_shift();

        if n_cache_reuse == 0 || !can_shift {
            if n_common < slot.cached_tokens.len() {
                debug_assert_eq!(self.context_mode(), ContextMode::Kv);

                // truncate kv and cached tokens to n_common length
                let removed = self
                    .llm_ctx_mut()
                    .memory_seq_rm(seq_id as i32, n_common as i32, -1);

                // if !removed {
                //     // For hybrid models (e.g. Qwen3.5), the cache has both attention
                //     // and recurrent layers.  The recurrent state does not support
                //     // arbitrary partial truncation — it can only roll back within
                //     // n_rs_seq steps (default 0).  When memory_seq_rm fails, we
                //     // retry with a truncated range bounded by n_rs_seq.  If that
                //     // also fails, try checkpoint restore before falling back to
                //     // full clear.
                //     let n_rs = self.llm_ctx().n_rs_seq() as usize;
                //     if n_rs > 0 {
                //         let n_clip = n_common.min(n_rs);
                //         log::debug!(
                //             "[prefill_slot] seq_rm failed for seq_id={}, \
                //          retrying with n_rs_seq={} clip (n_common={} -> n_clip={})",
                //             seq_id,
                //             n_rs,
                //             n_common,
                //             n_clip,
                //         );

                //         n_common = n_clip;
                //         removed =
                //             self.llm_ctx_mut()
                //                 .memory_seq_rm(seq_id as i32, n_clip as i32, -1);
                //     }
                // }

                if !removed {
                    log::debug!("[prefill_slot] seq_rm failed, reset slot.");
                    self.clear_slot(slot);
                    return Ok(0);
                }

                slot.cached_tokens.truncate(n_common);
            }

            return Ok(n_common);
        }

        //
        // KV shifting — scan for additional matching chunks
        //

        debug_assert_eq!(self.context_mode(), ContextMode::Kv);
        log::debug!(
            "[prepare_slot] shift kv chunks for reuse: n_cache_reuse={}",
            n_cache_reuse
        );

        let old_cache_len = slot.cached_tokens.len();
        let mut n_past = n_common;
        let mut head_c = n_common; // index to cached tokens
        let mut head_p = n_common; // index to prompt tokens

        while head_c < old_cache_len && head_p < prompt_tokens.len() {
            let mut n_match = 0usize;
            while head_c + n_match < old_cache_len
                && head_p + n_match < prompt_tokens.len()
                && slot.cached_tokens[head_c + n_match] == prompt_tokens[head_p + n_match]
            {
                n_match += 1;
            }

            if n_match >= n_cache_reuse {
                let head_p_i32 = head_p as i32;
                let head_c_i32 = head_c as i32;
                let kv_shift = head_p_i32 - head_c_i32;

                let llm = self.llm_ctx_mut();
                let r = llm.memory_seq_rm(seq_id as i32, head_p_i32, head_c_i32);
                debug_assert!(r);

                llm.memory_seq_add(
                    seq_id as i32,
                    head_c_i32,
                    (head_c + n_match) as i32,
                    kv_shift,
                );

                // cached_tokens may need to grow when the matching chunk
                // is shifted to a position beyond the current cache length
                // (head_p + n_match > cache_len).
                if head_p + n_match > slot.cached_tokens.len() {
                    slot.cached_tokens.resize(head_p + n_match, 0);
                }
                for i in 0..n_match {
                    slot.cached_tokens[head_p + i] = slot.cached_tokens[head_c + i];
                }

                n_past += n_match;
                head_c += n_match;
                head_p += n_match;
            } else {
                head_c += 1;
            }
        }

        slot.cached_tokens.truncate(n_past);
        Ok(n_past)
    }

    /// Perform context shift when KV cache is full.
    ///
    /// Keeps the first `n_keep` tokens, discards `n_discard` tokens after them,
    /// and shifts the remaining tokens forward.  This frees up space in the KV
    /// cache so generation can continue without recomputing the kept prefix.
    ///
    /// `n_discard` defaults to `(total - n_keep) / 2` when 0.
    /// Returns the new n_past value after the shift.
    pub(crate) fn context_shift(
        &mut self,
        slot: &mut Slot,
        n_keep: usize,
        n_discard: usize,
    ) -> Result<i32, String> {
        let seq_id = slot.seq_id;
        let cached = slot.cached_tokens.clone();
        let total = cached.len();
        if total <= n_keep {
            return Ok(total as i32);
        }

        let n_discard = if n_discard > 0 {
            n_discard
        } else {
            (total - n_keep) / 2
        };
        let n_discard = n_discard.min(total - n_keep);

        let llm = self.llm_ctx_mut();

        // Remove the discarded range [n_keep, n_keep + n_discard)
        llm.memory_seq_rm(seq_id as i32, n_keep as i32, (n_keep + n_discard) as i32);
        // Shift the remaining tokens [n_keep + n_discard .. total) backward by n_discard
        llm.memory_seq_add(
            seq_id as i32,
            (n_keep + n_discard) as i32,
            total as i32,
            -(n_discard as i32),
        );

        // Update cached_tokens: compact the token list
        let slot_tokens = &mut slot.cached_tokens;
        for i in n_keep..(total - n_discard) {
            slot_tokens[i] = cached[i + n_discard];
        }
        slot_tokens.truncate(total - n_discard);

        let new_n_past = (total - n_discard) as i32;
        Ok(new_n_past)
    }

    /// reset state for given slot id
    pub(crate) fn reset_sampler(&mut self, slot: &mut Slot) {
        slot.sampler = None;
    }

    pub(crate) fn clear_slot(&mut self, slot: &mut Slot) {
        self.llm_ctx_mut().memory_seq_rm(slot.seq_id as i32, 0, -1);
        slot.cached_tokens.clear();
        slot.sampler = None;
        slot.last_dump_tokens = 0;
        slot.checkpoint_dump_at = 0;
    }
}

fn common_prefix_len(cached_tokens: &[Token], prompt_tokens: &[Token]) -> usize {
    let max_cnt = cached_tokens.len().min(prompt_tokens.len());
    let mut n = 0usize;
    while n < max_cnt && cached_tokens[n] == prompt_tokens[n] {
        n += 1;
    }
    n
}
