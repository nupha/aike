use super::{Engine, slot::Slot, task::Token};

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

    /// Find the best matching entry for a given prompt.
    /// Returns (entry_index, common_len) for the best match,
    /// or None if no entry has any common prefix.
    pub fn find_best_match(
        &self,
        prompt_tokens: &[Token],
        match_threshold: f32,
    ) -> Option<(usize, usize)> {
        if self.max_bytes == 0 {
            return None;
        }

        let mut best_idx = None;
        let mut best_n = 0usize;

        for (i, entry) in self.entries.iter().enumerate() {
            let max_len = entry.tokens.len().min(prompt_tokens.len());
            let mut n = 0usize;
            while n < max_len && entry.tokens[n] == prompt_tokens[n] {
                n += 1;
            }
            // Apply threshold: either disabled (<=0) or ratio >= threshold
            if n > best_n
                && (match_threshold <= 0.0
                    || n as f32 / prompt_tokens.len() as f32 >= match_threshold)
            {
                best_n = n;
                best_idx = Some(i);
            }
        }

        best_idx.map(|i| (i, best_n))
    }

    /// Save a KV cache state into the RAM pool.
    /// Returns the index assigned to this entry.
    pub fn save(&mut self, kv_state: Box<[u8]>, tokens: Vec<Token>, mtime: i64) -> usize {
        let size = kv_state.len();
        if size == 0 || self.max_bytes == 0 {
            return usize::MAX; // disabled
        }

        // Evict LRU entries if needed to make room
        while self.total_bytes + size > self.max_bytes && !self.entries.is_empty() {
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
            log::debug!("[cache_ram] entry too large ({}), skipping", size);
            return usize::MAX;
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
    pub(crate) fn dump_slot_to_cache_ram(&mut self, slot: &mut Slot) {
        let tokens = std::mem::take(&mut slot.cached_tokens);

        if !tokens.is_empty() {
            let kv = self.llm_ctx().dump_seq_state(slot.seq_id as i32);
            if !kv.is_empty() {
                let r = self
                    .cache_ram
                    .save(kv, tokens, crate::helpers::now_unix_s());
                debug_assert!(r != usize::MAX, "fail to save cache_ram entry");

                self.clear_slot(slot);
            }
        }
    }

    /// Restore a cache_ram entry into a slot, keeping only the first `n_tokens`
    /// tokens of the cached KV state.  Returns the number of bytes restored,
    /// or 0 on failure.
    ///
    /// `n_tokens` must be <= the number of tokens in the cached entry.
    /// Since `dump_seq_state` serializes the entire sequence as an opaque blob,
    /// we restore the full state first, then truncate via `memory_seq_rm`.
    pub(crate) fn restore_cache_ram_to_slot(
        &mut self,
        slot: &mut Slot,
        cache_idx: usize,
        n_tokens: usize,
    ) -> usize {
        // Get the state bytes and tokens from cache_ram (borrows self immutably)
        let (state_ptr, state_len, cached_tokens) = {
            let (s, t) = match self.cache_ram.get(cache_idx) {
                Some(v) => v,
                None => return 0,
            };
            (s.as_ptr(), s.len(), t.to_vec())
        };

        let n_tokens = n_tokens.min(cached_tokens.len());

        // Clear the slot and restore
        self.clear_slot(slot);

        let state = unsafe { std::slice::from_raw_parts(state_ptr, state_len) };

        let restored = self
            .llm_ctx_mut()
            .restore_seq_state(slot.seq_id as i32, state);

        if restored > 0 {
            slot.cached_tokens = cached_tokens;
            self.cache_ram
                .touch(cache_idx, crate::helpers::now_unix_s());

            if n_tokens > 0 {
                // Truncate to n_tokens — remove tokens [n_tokens ..)
                self.llm_ctx_mut()
                    .memory_seq_rm(slot.seq_id as i32, n_tokens as i32, -1);
                slot.cached_tokens.truncate(n_tokens);
            }
        }

        restored
    }
}
