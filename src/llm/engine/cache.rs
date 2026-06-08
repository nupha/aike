use super::{Engine, slot::Slot, task::Token};

pub(crate) const CHECKPOINT_STEP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextMode {
    Kv,         // attention 模型：restore + seq_rm 截断
    Checkpoint, // hybrid 模型：restore，不截断
}

#[derive(Debug)]
struct CacheEntry {
    kv_state: Box<[u8]>,
    tokens: Vec<Token>,
    mtime: i64,
    size: usize,
}

/// RAM-based prompt cache pool.
///
/// When a slot without sess_id finishes inference, its KV cache state is
/// serialized and stored here.  On the next request, the pool is searched
/// for the best prefix match; if found, the state is restored into a slot,
/// avoiding recomputation of the common prefix.
///
/// Entries are evicted by LRU when the total size exceeds `max_bytes`.
#[derive(Debug)]
pub(crate) struct CacheRam {
    /// Map from seq_id (virtual) to cached entry.
    entries: Vec<CacheEntry>,
    /// Maximum total size in bytes (0 = disabled).
    max_bytes: usize,
    /// Current total size of all cached states.
    total_bytes: usize,
}

impl CacheRam {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            entries: Vec::with_capacity(8),
            max_bytes,
            total_bytes: 0,
        }
    }

    /// Number of entries in the cache.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Find the best matching entry for a given prompt.
    /// Returns (entry_index, common_len) for the best match,
    /// or None if no entry has any common prefix.
    pub fn find_best_match(
        &self,
        prompt_tokens: &[Token],
        cache_mode: ContextMode,
    ) -> Option<(usize, usize)> {
        let mut best_idx = None;
        let mut best_n = 0usize;

        match cache_mode {
            ContextMode::Checkpoint => {
                // Only full entry match is valid — entry.tokens must be a prefix
                // of prompt_tokens.  Prefer the longest matching entry.
                for (i, entry) in self.entries.iter().enumerate() {
                    let entry_len = entry.tokens.len();
                    if entry_len > prompt_tokens.len() {
                        log::debug!(
                            "[cache_ram] entry#{} skip: entry_len={} > prompt_len={}",
                            i, entry_len, prompt_tokens.len(),
                        );
                        continue;
                    }
                    // Find first mismatch position
                    let mut common = 0;
                    while common < entry_len && entry.tokens[common] == prompt_tokens[common] {
                        common += 1;
                    }
                    if common < entry_len {
                        log::debug!(
                            "[cache_ram] entry#{} mismatch at pos {}: entry_token={} prompt_token={} (entry_len={}, prompt_len={})",
                            i, common, entry.tokens[common], prompt_tokens[common],
                            entry_len, prompt_tokens.len(),
                        );
                    }
                    if common == entry_len && entry_len > best_n {
                        best_n = entry_len;
                        best_idx = Some(i);
                    }
                }
            }
            ContextMode::Kv => {
                // Longest common prefix (partial match OK).
                for (i, entry) in self.entries.iter().enumerate() {
                    let max_len = entry.tokens.len().min(prompt_tokens.len());
                    let mut n = 0usize;
                    while n < max_len && entry.tokens[n] == prompt_tokens[n] {
                        n += 1;
                    }
                    if n > best_n {
                        best_n = n;
                        best_idx = Some(i);
                    }
                }
            }
        }

        best_idx.map(|i| (i, best_n))
    }

    /// Save a KV cache state into the RAM pool.
    /// Returns the index assigned to this entry.
    pub fn save(&mut self, kv_state: Box<[u8]>, tokens: Vec<Token>, mtime: i64) -> usize {
        let size = kv_state.len();
        if size == 0 {
            return usize::MAX; // disabled
        }

        // Evict LRU entries if needed to make room
        if self.max_bytes > 0 {
            while self.total_bytes + size > self.max_bytes {
                if let Some(idx) = self
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.mtime)
                    .map(|(i, _)| i)
                {
                    self.evict(idx);
                } else {
                    break;
                }
            }

            // If still too large after eviction, skip saving
            if self.total_bytes + size > self.max_bytes {
                log::debug!(
                    "[cache_ram] entry too large ({}) max:{}, skipping",
                    self.total_bytes + size,
                    self.max_bytes
                );
                return usize::MAX;
            }
        }

        self.entries.push(CacheEntry {
            kv_state,
            tokens,
            mtime,
            size,
        });

        let idx = self.entries.len() - 1;
        self.total_bytes += size;

        log::debug!(
            "[cache_ram] saved entry id={} size={} total={}",
            idx,
            size,
            self.total_bytes,
        );
        idx
    }

    /// Remove an entry by its index
    fn evict(&mut self, idx: usize) {
        debug_assert!(idx < self.entries.len());
        let entry = self.entries.swap_remove(idx);
        self.total_bytes -= entry.size;
        log::debug!(
            "[cache_ram] evicted entry id={} size={} total={}",
            idx,
            entry.size,
            self.total_bytes,
        );
    }

    /// Get the serialized state and tokens for a index
    pub fn get(&self, index: usize) -> Option<(&[u8], &[Token])> {
        self.entries
            .get(index)
            .map(|e| (e.kv_state.as_ref(), e.tokens.as_slice()))
    }

    /// Update the mtime of an entry (mark as recently used).
    pub fn touch(&mut self, index: usize, mtime: i64) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.mtime = mtime;
        }
    }
}

impl Engine {
    /// Dump slot kv and tokens to cache_ram, and clear the slot cache and tokens.
    pub(crate) fn dump_slot_to_cache_ram(&mut self, slot: &mut Slot, reset_after_dump: bool) {
        if slot.cached_tokens.is_empty() {
            return;
        }

        let mode = self.context_mode();

        if mode == ContextMode::Checkpoint {
            if slot.cached_tokens.len() < slot.last_dump_tokens + CHECKPOINT_STEP {
                log::debug!(
                    "Skip dump slot #{} ({} tokens) to cache: too close to last dump {} (step={})",
                    slot.seq_id,
                    slot.cached_tokens.len(),
                    slot.last_dump_tokens,
                    CHECKPOINT_STEP,
                );
                return;
            }
        }

        let state = self.llm_ctx().dump_seq_state(slot.seq_id as i32);
        if state.is_empty() {
            return;
        }

        log::debug!(
            "Dump slot #{} ({} tokens, last_dump={}) to cache (mode={:?}) first10={:?}",
            slot.seq_id,
            slot.cached_tokens.len(),
            slot.last_dump_tokens,
            mode,
            &slot.cached_tokens[..slot.cached_tokens.len().min(10)],
        );

        let r = self.cache_ram.save(
            state,
            if reset_after_dump {
                std::mem::take(&mut slot.cached_tokens)
            } else {
                slot.cached_tokens.clone()
            },
            crate::helpers::now_unix_s(),
        );

        if r != usize::MAX && mode == ContextMode::Checkpoint {
            slot.last_dump_tokens = slot.cached_tokens.len();
        }

        if r != usize::MAX {
            if reset_after_dump {
                self.clear_slot(slot);
            }
        }
    }

    /// Restore a cache_ram entry into a slot, keeping only the first `n_tokens`
    /// tokens of the cached KV state.  Returns the number of bytes restored,
    /// or 0 on failure.
    ///
    /// `truncate` must be <= the number of tokens in the cached entry.
    /// Since `dump_seq_state` serializes the entire sequence as an opaque blob,
    /// we restore the full state first, then truncate via `memory_seq_rm`.
    pub(crate) fn restore_cache_ram_to_slot(
        &mut self,
        slot: &mut Slot,
        cache_idx: usize,
        truncate: usize,
    ) -> usize {
        debug_assert!(cache_idx < self.cache_ram.entries.len());

        let mode = self.context_mode();
        log::debug!(
            "Restore cached ram #{} to slot #{} (mode={:?})",
            cache_idx,
            slot.seq_id,
            mode
        );

        // Clear the slot first
        self.clear_slot(slot);

        // Get the state bytes and tokens from cache_ram (borrows self immutably)
        let (state_ptr, state_len, cached_tokens) = {
            let (s, t) = &self.cache_ram.get(cache_idx).unwrap();
            (s.as_ptr(), s.len(), t.to_vec())
        };

        #[cfg(debug_assertions)]
        if mode == ContextMode::Checkpoint {
            // In Checkpoint mode, truncate must equal the entry's token count
            // (we restore the full entry, no truncation).
            debug_assert_eq!(truncate, cached_tokens.len(),
                "Checkpoint restore: truncate={} != entry tokens={}",
                truncate, cached_tokens.len());
        }

        let state = unsafe { std::slice::from_raw_parts(state_ptr, state_len) };

        let restored = self
            .llm_ctx_mut()
            .restore_seq_state(slot.seq_id as i32, state);

        if restored > 0 {
            slot.cached_tokens = cached_tokens;
            self.cache_ram
                .touch(cache_idx, crate::helpers::now_unix_s());

            match mode {
                ContextMode::Kv => {
                    if truncate < slot.cached_tokens.len() {
                        self.llm_ctx_mut()
                            .memory_seq_rm(slot.seq_id as i32, truncate as i32, -1);
                        slot.cached_tokens.truncate(truncate);
                    }
                }
                ContextMode::Checkpoint => {
                    // Checkpoint mode: no truncation, keep all tokens
                    slot.last_dump_tokens = slot.cached_tokens.len();
                }
            }
        }

        restored
    }
}
