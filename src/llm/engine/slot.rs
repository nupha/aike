use std::collections::VecDeque;

use llama_cpp_ffi::SamplerChain;

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
    /// Cached tokens for this slot, used for prefix matching and
    /// for dumping/restoring KV cache state to/from CacheRam.
    pub cached_tokens: Vec<Token>,
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
        let mut best_idx = None;
        let mut best_n = 0usize;
        let mut lru_idx = None;
        let mut lru_mtime = i64::MAX;

        for (idx, slot) in slots.iter().enumerate() {
            if matches!(slot.state, SlotState::Idle) {
                let n = common_prefix_len(&slot.cached_tokens, prompt_tokens);

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
                    lru_idx = Some(idx);
                }
            }
        }

        if let Some(best) = best_idx {
            Some((best, best_n))
        } else if let Some(lru) = lru_idx {
            self.dump_slot_to_cache_ram(&mut slots[lru]);
            Some((lru, 0))
        } else {
            None
        }
    }

    /// Align KV/cache state for prefix reuse. Returns reusable prefix length.
    /// `n_common` is the raw common prefix length (from `pick_idle_slot`); this
    /// function adjusts it so that at least one token remains for the model to
    /// process.
    pub(super) fn prefill_slot(
        &mut self,
        slot: &mut Slot,
        prompt_tokens: &[Token],
        n_common: usize,
    ) -> Result<usize, String> {
        let seq_id = slot.seq_id;

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
            self.clear_slot(slot);
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
                        slot.cached_tokens.truncate(n_clip);
                        return Ok(n_clip);
                    }
                }

                log::debug!(
                    "[prepare_slot] seq_rm failed (hybrid model? n_rs_seq={n_rs} too small?), \
                     falling back to full clear for seq_id={seq_id}"
                );

                self.clear_slot(slot);
                Ok(0)
            } else {
                slot.cached_tokens.truncate(n_common);
                Ok(n_common)
            }
        }
    }

    pub(crate) fn truncate_slot_cache(&mut self, slot: &mut Slot, n_keep: usize) {
        slot.cached_tokens.truncate(n_keep);
    }

    /// Align KV/cache state for prefix reuse with KV shifting (cache_reuse).
    ///
    /// Like `prepare_slot`, but after the initial prefix match, scans the
    /// remaining cached tokens for additional matching chunks and uses
    /// `memory_seq_rm` + `memory_seq_add` to shift them into the correct
    /// position.  This avoids recomputing tokens that appear later in the
    /// new prompt but at different positions.
    ///
    /// `n_common` is the initial common prefix length (from `pick_idle_slot`).
    /// `n_cache_reuse` is the minimum chunk size to attempt shifting (0 = disabled).
    /// Returns the total number of reusable tokens (n_past).
    pub(super) fn prefill_slot_with_shift(
        &mut self,
        slot: &mut Slot,
        prompt_tokens: &[Token],
        n_common: usize,
        n_cache_reuse: usize,
    ) -> Result<usize, String> {
        let seq_id = slot.seq_id;

        // Step 1: basic prefix match (same as prepare_slot)
        let n_common = if n_common == prompt_tokens.len() && n_common > 0 {
            n_common - 1
        } else {
            n_common
        };

        if n_common == 0 {
            self.clear_slot(slot);
            return Ok(0);
        }

        // Check if KV shifting is supported
        let can_shift = n_cache_reuse > 0 && self.llm_ctx().memory_can_shift();

        if !can_shift {
            // Fall back to basic prepare_slot
            let removed = self
                .llm_ctx_mut()
                .memory_seq_rm(seq_id as i32, n_common as i32, -1);
            if !removed {
                self.clear_slot(slot);
                return Ok(0);
            }
            slot.cached_tokens.truncate(n_common);
            return Ok(n_common);
        }

        // Step 2: remove tokens after n_common from cache
        let removed = self
            .llm_ctx_mut()
            .memory_seq_rm(seq_id as i32, n_common as i32, -1);
        if !removed {
            self.clear_slot(slot);
            return Ok(0);
        }
        slot.cached_tokens.truncate(n_common);

        // Step 3: KV shifting — scan for additional matching chunks
        let cached = slot.cached_tokens.clone();
        let mut n_past = n_common;
        let mut head_c = n_common; // position in cached tokens
        let mut head_p = n_common; // position in new prompt tokens

        while head_c < cached.len() && head_p < prompt_tokens.len() {
            // Count matching tokens from current positions
            let mut n_match = 0usize;
            while head_c + n_match < cached.len()
                && head_p + n_match < prompt_tokens.len()
                && cached[head_c + n_match] == prompt_tokens[head_p + n_match]
            {
                n_match += 1;
            }

            if n_match >= n_cache_reuse {
                // Shift the matching chunk from [head_c, head_c+n_match) to [head_p, head_p+n_match)
                let head_p_i32 = head_p as i32;
                let head_c_i32 = head_c as i32;
                let kv_shift = head_p_i32 - head_c_i32;

                // Remove any stale tokens at the target position, then shift
                let llm = self.llm_ctx_mut();
                llm.memory_seq_rm(seq_id as i32, head_p_i32, head_c_i32);
                llm.memory_seq_add(
                    seq_id as i32,
                    head_c_i32,
                    (head_c + n_match) as i32,
                    kv_shift,
                );
                // llm dropped here

                // Update cached_tokens: copy the matched tokens into position
                let slot_tokens = &mut slot.cached_tokens;
                for i in 0..n_match {
                    if head_p + i < slot_tokens.len() {
                        slot_tokens[head_p + i] = cached[head_c + i];
                    } else {
                        slot_tokens.push(cached[head_c + i]);
                    }
                }

                n_past += n_match;
                head_c += n_match;
                head_p += n_match;
            } else {
                head_c += 1;
            }
        }

        // Truncate cached tokens to n_past (remove anything beyond what we kept)
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
