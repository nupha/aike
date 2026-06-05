//! Context checkpoint for FULL-removal memory (recurrent/hybrid models).
//!
//! Attention models can use `memory_seq_rm` + `memory_seq_add` to freely
//! truncate and shift KV cache.  Recurrent/hybrid models (Mamba, RWKV,
//! Qwen3.5, etc.) report `COMMON_CONTEXT_SEQ_RM_TYPE_FULL` — they cannot
//! partially erase a sequence.  For these models, we save periodic
//! checkpoints of the full KV+recurrent state during prefill, and restore
//! from the nearest checkpoint on the next request instead of truncating.
//!
//! Design follows llama.cpp PR #22932:
//!   - Checkpoints are created *before* `llama_decode()` during prefill,
//!     so the current batch is NOT included in the checkpoint.
//!   - On restore, we use `LLAMA_STATE_SEQ_FLAGS_PARTIAL_ONLY` to load
//!     only the recurrent (non-attention) portion of the state.
//!   - The checkpoint pool is per-slot, with LRU eviction when the pool
//!     exceeds `max_checkpoints`.

use crate::helpers::now_unix_s;

use super::Engine;

/// Step size for checkpoint spacing and matching tolerance.
///
/// - When saving: a new checkpoint is created only if its `n_tokens` is
///   at least this many tokens away from the newest checkpoint.
/// - When finding: a checkpoint is considered usable if its `n_tokens`
///   is within this many tokens of the requested `n_past`.
///
/// A single constant ensures save density and match tolerance stay in
/// sync — checkpoints are spaced far enough apart to be useful, and
/// the matching window is wide enough to find a suitable candidate.
const CHECKPOINT_STEP: i64 = 128;

/// A single checkpoint capturing the full state of a slot at a given
/// token position.
#[derive(Debug)]
pub(crate) struct Checkpoint {
    /// Serialized KV cache + recurrent state (opaque blob).
    pub data: Box<[u8]>,
    /// Number of prompt tokens processed when this checkpoint was created.
    pub n_tokens: i64,
    /// Minimum position in the KV cache at checkpoint time.
    pub pos_min: i32,
    /// Maximum position in the KV cache at checkpoint time.
    pub pos_max: i32,
    /// Timestamp (unix seconds) for LRU eviction.
    pub mtime: i64,
}

/// Per-slot checkpoint pool.
///
/// Each slot maintains a list of checkpoints taken at different token
/// positions during prefill.  Multiple checkpoints allow matching
/// requests with varying prefix lengths — a shorter checkpoint can
/// serve a short request, while a longer one serves a long request.
/// When the pool is full, the oldest checkpoint is evicted (LRU).
#[derive(Debug)]
pub(crate) struct CheckpointPool {
    /// Ordered from oldest (front) to newest (back).
    checkpoints: Vec<Checkpoint>,
    /// Maximum number of checkpoints to keep per slot (0 = disabled).
    max_checkpoints: usize,
}

impl CheckpointPool {
    pub fn new(max_checkpoints: usize) -> Self {
        Self {
            checkpoints: Vec::with_capacity(max_checkpoints.min(8)),
            max_checkpoints,
        }
    }

    /// Returns true if checkpointing is enabled.
    pub fn is_enabled(&self) -> bool {
        self.max_checkpoints > 0
    }

    /// Save a new checkpoint.
    ///
    /// If the new checkpoint's `n_tokens` is smaller than the smallest
    /// existing checkpoint, it indicates a different session — all
    /// existing checkpoints are cleared first.
    ///
    /// Otherwise, evicts the oldest checkpoint if the pool is at capacity.
    /// Skips if a checkpoint at this exact `n_tokens` already exists,
    /// or if the newest checkpoint is fewer than `CHECKPOINT_STEP`
    /// tokens away (prevents overly dense checkpoints).
    pub fn save(&mut self, data: Box<[u8]>, n_tokens: i64, pos_min: i32, pos_max: i32) -> bool {
        if !self.is_enabled() || data.is_empty() {
            return false;
        }

        // If the new checkpoint is smaller than the smallest existing one,
        // this is likely a different session — clear all old checkpoints.
        if let Some(smallest) = self.checkpoints.first() {
            if n_tokens < smallest.n_tokens {
                log::debug!(
                    "[checkpoint] new n_tokens={} < smallest {} — different session, clearing all",
                    n_tokens,
                    smallest.n_tokens,
                );
                self.checkpoints.clear();
            }
        }

        // Skip if we already have a checkpoint at this exact n_tokens
        if self.checkpoints.iter().any(|cp| cp.n_tokens == n_tokens) {
            return false;
        }

        // Skip if too close to the newest checkpoint
        if let Some(newest) = self.checkpoints.last() {
            if n_tokens < newest.n_tokens + CHECKPOINT_STEP {
                log::debug!(
                    "[checkpoint] skipping save: n_tokens={} too close to newest {} (step={})",
                    n_tokens,
                    newest.n_tokens,
                    CHECKPOINT_STEP,
                );
                return false;
            }
        }

        // Evict oldest if at capacity
        while self.checkpoints.len() >= self.max_checkpoints {
            self.checkpoints.remove(0);
        }

        self.checkpoints.push(Checkpoint {
            data,
            n_tokens,
            pos_min,
            pos_max,
            mtime: now_unix_s(),
        });
        true
    }

    /// Find the best checkpoint for a given `n_past`.
    ///
    /// Returns the checkpoint with the largest `n_tokens` that satisfies
    /// `n_tokens <= max_n_tokens + 1`.  The +1 tolerance accounts for the
    /// `n_common - 1` adjustment in `prefill_slot` when `n_common == tokens.len()`.
    /// If none exists, returns `None` and the caller falls back to full prefill.
    pub fn find(&self, max_n_tokens: i64) -> Option<&Checkpoint> {
        let result = self
            .checkpoints
            .iter()
            .filter(|cp| cp.n_tokens <= max_n_tokens + 1)
            .max_by_key(|cp| cp.n_tokens);

        if result.is_none() {
            log::debug!(
                "[checkpoint] find(n_tokens<={}) failed, available checkpoints: {:?}",
                max_n_tokens,
                self.checkpoints
                    .iter()
                    .map(|cp| cp.n_tokens)
                    .collect::<Vec<_>>(),
            );
        }
        result
    }

    /// Invalidate checkpoints that are no longer reachable.
    pub fn invalidate(&mut self, n_past: i64) {
        self.checkpoints.retain(|cp| cp.n_tokens <= n_past);
    }

    /// Clear all checkpoints.
    pub fn clear(&mut self) {
        self.checkpoints.clear();
    }

    /// Number of checkpoints currently stored.
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }
}

impl Engine {
    /// Check whether checkpoint-based prompt cache should be used for the
    /// current context.
    ///
    /// Returns `true` when the memory type does NOT support partial seq_rm
    /// (i.e. recurrent/hybrid models), so we fall back to checkpoint restore.
    pub(crate) fn needs_checkpoint_cache(&self) -> bool {
        let ctx = self.llm_ctx();
        // When memory_can_shift is false, the model is likely recurrent/hybrid
        // and cannot do partial seq_rm.  We also check n_rs_seq: if it's 0,
        // partial rollback is not available either.
        !ctx.memory_can_shift() && ctx.n_rs_seq() == 0
    }

    /// Try to restore slot state from a checkpoint.
    ///
    /// Searches the slot's checkpoint pool for the best match with
    /// `n_tokens <= n_past`.  If found, restores the state and returns
    /// the restored `n_past`.  Returns `None` if no suitable checkpoint
    /// exists or restore fails.
    pub(crate) fn restore_from_checkpoint(
        &mut self,
        checkpoints: &mut CheckpointPool,
        seq_id: i32,
        n_past: i64,
    ) -> Option<i64> {
        let cp = checkpoints.find(n_past)?;

        log::debug!(
            "[checkpoint] restoring seq_id={} from checkpoint n_tokens={} (requested n_past={})",
            seq_id,
            cp.n_tokens,
            n_past,
        );

        let restored = self.llm_ctx_mut().restore_seq_state(seq_id, &cp.data);

        if restored != cp.data.len() {
            log::error!(
                "[checkpoint] failed to restore checkpoint (n_tokens={}, size={})",
                cp.n_tokens,
                cp.data.len(),
            );
            return None;
        }

        // Use the checkpoint's n_tokens as the new n_past, because the
        // checkpoint contains state for exactly that many tokens.  Using
        // min(n_past, cp.n_tokens) would undercount and cause position
        // conflicts on the next decode (M-RoPE requires X < Y).
        let new_n_past = cp.n_tokens as i64;
        log::debug!(
            "[checkpoint] restored from checkpoint n_tokens={} -> n_past={}",
            cp.n_tokens,
            new_n_past,
        );

        Some(new_n_past)
    }
}
