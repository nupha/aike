use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crossbeam::channel::Receiver;

use crate::llm::engine::cache::CHECKPOINT_STEP;

use super::{
    cache::{CacheRam, ContextMode},
    context::Engine,
    slot::{Slot, SlotState},
    task::{EngineCtrlMsg, EngineReply, EngineTask, SchedResult, StopReason},
};

use super::output::GeneratedContent;

/// Helper function to check if a GeneratedContent matches the parser state
fn matches_content_state(content: &GeneratedContent, state: super::output::OutputState) -> bool {
    matches!(
        (content, state),
        (GeneratedContent::Text(_), super::output::OutputState::Text)
            | (
                GeneratedContent::Reasoning(_),
                super::output::OutputState::Reasoning
            )
            | (
                GeneratedContent::ToolCall(_),
                super::output::OutputState::ToolCall
            )
    )
}

pub fn worker_loop(
    engine: Arc<Mutex<Engine>>,
    max_parallel: usize,
    idle_sleep: Duration,
    slot_match_threshold: f32,
    input_rx: Receiver<Box<EngineTask>>,
    ctrl_rx: Receiver<EngineCtrlMsg>,
) {
    let mut slots = (0..max_parallel).map(|i| Slot::new(i)).collect::<Vec<_>>();
    let mut slot_cursor = 0usize;

    let mut ready_tasks = VecDeque::<Box<EngineTask>>::new();
    let mut deferred_tasks = VecDeque::<Box<EngineTask>>::new();

    let mut cancelled_ids = HashSet::<usize>::new();

    loop {
        if engine.lock().unwrap().handle_ctrl_messages(
            &ctrl_rx,
            &mut slots,
            &mut ready_tasks,
            &mut deferred_tasks,
            &mut cancelled_ids,
        ) {
            engine.lock().unwrap().num_tasks = 0;
            return;
        }

        while let Ok(task) = input_rx.try_recv() {
            if deferred_tasks.is_empty() {
                ready_tasks.push_back(task);
            } else {
                deferred_tasks.push_back(task);
            }
        }

        while let Some(task) = pop_next_task(&mut ready_tasks, &mut deferred_tasks) {
            if cancelled_ids.remove(&task.config.task_id) {
                log::info!(
                    "[worker] task id={} was cancelled, sending Cancelled reply",
                    task.config.task_id
                );
                let _ = task.output_tx.send(EngineReply::Cancelled);
            } else {
                match engine
                    .lock()
                    .unwrap()
                    .sched_task(task, &mut slots, slot_match_threshold)
                {
                    SchedResult::Scheduled => {
                        log::debug!("[worker] task scheduled");
                    }
                    SchedResult::Deferred(task) => {
                        log::debug!(
                            "[worker] sched_task deferred, task id={}",
                            task.config.task_id
                        );
                        deferred_tasks.push_back(task);
                        break;
                    }
                    SchedResult::Busy(task) => {
                        log::warn!("[worker] sched_task busy, task id={}", task.config.task_id);
                        let _ = task.output_tx.send(EngineReply::Error(
                            "all slots are busy, try again later".to_string(),
                        ));
                    }
                    SchedResult::Rejected => {
                        log::warn!("[worker] sched_task rejected");
                    }
                }
            }
        }

        // tick engine pipeline
        let did_work = engine.lock().unwrap().tick(&mut slots, &mut slot_cursor);

        let num_tasks = slots
            .iter()
            .filter(|s| !matches!(s.state, SlotState::Idle))
            .count();
        engine.lock().unwrap().num_tasks = num_tasks;

        if !did_work {
            thread::sleep(idle_sleep);
        }
    }
}

fn pop_next_task(
    ready: &mut VecDeque<Box<EngineTask>>,
    deferred: &mut VecDeque<Box<EngineTask>>,
) -> Option<Box<EngineTask>> {
    if let Some(v) = ready.pop_front() {
        return Some(v);
    }
    if deferred.is_empty() {
        return None;
    }
    let mut i = 0usize;
    while i < deferred.len() {
        if let Some(v) = deferred.remove(i) {
            ready.push_back(v);
            continue;
        }
        i += 1;
    }
    ready.pop_front()
}

pub(super) fn remove_cancelled_inputs(queue: &mut VecDeque<Box<EngineTask>>, task_id: usize) {
    let mut kept = VecDeque::with_capacity(queue.len());
    while let Some(task) = queue.pop_front() {
        if task.config.task_id == task_id {
            let _ = task.output_tx.send(EngineReply::Cancelled);
        } else {
            kept.push_back(task);
        }
    }
    *queue = kept;
}

pub(super) fn hit_stop_words(generated_text: &str, stop_words: &[String]) -> Option<String> {
    for stop in stop_words.iter().filter(|s| !s.is_empty()) {
        if generated_text.ends_with(stop) {
            return Some(stop.to_string());
        }
    }
    None
}

impl Engine {
    fn decoding_stage(&mut self, slots: &mut [Slot], slot_cursor: &mut usize) -> bool {
        // Tracks slots whose prefill just completed (will enter Sampling in next tick).
        let mut prefill_done_slots: Vec<usize> = Vec::new();

        let mut has_sampling = false;
        let mut cnt = 0usize;
        let mut written_updates: Vec<(usize, usize)> = Vec::new();

        let slots_len = slots.len();

        self.begin_decode_batch();

        for offset in 0..slots_len {
            if cnt == self.ubatch_size {
                break; // ubatch full
            }

            let slot_idx = (*slot_cursor + offset) % slots_len;
            let slot = &mut slots[slot_idx];

            if matches!(slot.state, SlotState::Decoding { .. }) {
                let SlotState::Decoding {
                    mut prompt_tokens,
                    logits_for_all_tokens,
                    mut num_written,
                    task_state,
                    is_prefill,
                } = std::mem::replace(&mut slot.state, SlotState::Busy)
                else {
                    unreachable!();
                };

                let n_tokens = prompt_tokens.len();
                let n_take = n_tokens.min(self.ubatch_size.saturating_sub(cnt));
                let mut write_count = 0usize;
                let mut decode_err: Option<String> = None;

                for token in prompt_tokens.drain(..n_take) {
                    let need_logits = task_state.config.logits_for_all_tokens || n_take == n_tokens;
                    match self.push_decode_token(
                        slot,
                        token,
                        slot.n_past + write_count as i32,
                        need_logits,
                    ) {
                        Ok(_) => {
                            num_written += 1;
                            write_count += 1;
                            cnt += 1;
                        }
                        Err(e) => {
                            decode_err = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = decode_err {
                    slot.cached_tokens.truncate(slot.n_past as usize);
                    slot.state = SlotState::Done {
                        task_state,
                        reply: EngineReply::Error(e),
                    };
                    continue;
                }

                slot.state = if prompt_tokens.is_empty() {
                    // prefill done, will enter sampling/done phase
                    if is_prefill {
                        prefill_done_slots.push(slot_idx);
                    }

                    if logits_for_all_tokens {
                        SlotState::Done {
                            task_state,
                            reply: EngineReply::Stop(StopReason::PrefillOnly, 0),
                        }
                    } else {
                        has_sampling = true;
                        SlotState::Sampling {
                            logits: cnt as i32 - 1,
                            task_state,
                        }
                    }
                } else {
                    // go on prefilling next
                    SlotState::Decoding {
                        prompt_tokens,
                        logits_for_all_tokens,
                        num_written,
                        task_state,
                        is_prefill,
                    }
                };

                if write_count > 0 {
                    written_updates.push((slot_idx, write_count));
                }
            }
        }

        if cnt > 0 {
            match self.decode_batch() {
                Ok(_) => {
                    for (idx, wrote) in &written_updates {
                        slots[*idx].n_past += *wrote as i32;
                    }

                    // Dumnp prefill checkpoints to cache ram for recurrent/hybrid models
                    // after prefill completes.
                    // We only save once per slot, when it transitions from
                    // Decoding to Sampling (i.e. all prompt tokens processed).
                    // Must run after n_past update so that n_tokens matches
                    // the actual number of processed tokens.
                    if self.context_mode() == ContextMode::Checkpoint {
                        for &idx in &prefill_done_slots {
                            let slot = &mut slots[idx];
                            if slot.cached_tokens.len() >= slot.last_dump_tokens + CHECKPOINT_STEP {
                                self.dump_slot_to_cache_ram(slot, false);
                            }
                        }
                    }
                }
                // Err(e) if e == "ContextFull" => {
                //     todo!("kv cache full, need to shift context");
                // }
                Err(e) => {
                    for (idx, _) in &written_updates {
                        let slot = &mut slots[*idx];

                        slot.cached_tokens.truncate(slot.n_past as usize);
                        self.llm_ctx_mut()
                            .memory_seq_rm(slot.seq_id as _, slot.n_past as _, -1);

                        let old = std::mem::replace(&mut slot.state, SlotState::Busy);
                        slot.state = match old {
                            SlotState::Decoding { task_state, .. }
                            | SlotState::Sampling { task_state, .. } => SlotState::Done {
                                task_state,
                                reply: EngineReply::Error(e.clone()),
                            },
                            other => other,
                        };
                    }
                    has_sampling = false;
                }
            }
        }

        if slots_len > 0 {
            *slot_cursor = (*slot_cursor + 1) % slots_len;
        }

        has_sampling
    }

    fn sampling_stage(&mut self, slots: &mut [Slot]) -> bool {
        let mut did_work = false;
        let model = self.llm_ctx().model().clone();

        for i in 0..slots.len() {
            if matches!(slots[i].state, SlotState::Sampling { .. }) {
                let SlotState::Sampling {
                    logits,
                    mut task_state,
                } = std::mem::replace(&mut slots[i].state, SlotState::Busy)
                else {
                    unreachable!();
                };

                if task_state.output_tx.is_full() {
                    // channel is full, try sampling in next tick
                    slots[i].state = SlotState::Sampling { logits, task_state };
                    continue;
                }

                if let Err(e) = self.init_sampler(
                    &mut slots[i],
                    task_state.config.temperature,
                    task_state.config.top_p,
                    task_state.config.grammar.as_ref(),
                ) {
                    // can't init sampler
                    slots[i].state = SlotState::Done {
                        task_state,
                        reply: EngineReply::Error(e),
                    };
                    continue;
                }

                did_work = true;

                match self.sample(&mut slots[i], logits) {
                    Ok(token) => {
                        self.sample_accept(&mut slots[i], token);

                        if model.is_eog(token) {
                            let n = task_state.n_completion_tokens;
                            slots[i].state = SlotState::Done {
                                task_state,
                                reply: EngineReply::Stop(StopReason::Eog, n),
                            };
                            continue;
                        }

                        if task_state.config.max_tokens > 0
                            && task_state.n_completion_tokens >= task_state.config.max_tokens
                        {
                            let n = task_state.n_completion_tokens;
                            slots[i].state = SlotState::Done {
                                task_state,
                                reply: EngineReply::Stop(StopReason::MaxTokens, n),
                            };
                            continue;
                        }

                        task_state.add_output_token(token, self.model());

                        #[cfg(debug_assertions)]
                        {
                            if let Some(parser) = &task_state.output_parser {
                                debug_assert!(
                                    matches_content_state(
                                        task_state.current_content.last().unwrap(),
                                        parser.current_state()
                                    ),
                                    "current_content state mismatch: {:?} != {:?}",
                                    task_state.current_content.last().unwrap(),
                                    parser.current_state()
                                );
                            }

                            if task_state.config.output_stream {
                                debug_assert_eq!(
                                    task_state.current_content.len(),
                                    1,
                                    "streaming mode should have only 1 content element in sampling_stage"
                                );
                            }
                        }

                        if let Some(last) = task_state.current_content.last_mut()
                            && let Some(w) =
                                hit_stop_words(last.as_str(), &task_state.config.stop_words)
                        {
                            // Found stop words - truncate stop word from tail of last content's text
                            let new_len = last.as_str().len().saturating_sub(w.len());
                            last.truncate(new_len);

                            let n = task_state.n_completion_tokens;
                            slots[i].state = SlotState::Done {
                                task_state,
                                reply: EngineReply::Stop(StopReason::Word(w), n),
                            };
                            continue;
                        }

                        if task_state.config.output_stream
                            && let Some(last) = task_state.current_content.last()
                            && !matches!(last, GeneratedContent::ToolCall(_))
                            && !last.as_str().is_empty()
                        {
                            let content = task_state.current_content.pop().unwrap();

                            // Determine placeholder type before sending
                            let empty = match content {
                                GeneratedContent::Text(ref _s) => {
                                    GeneratedContent::Text(String::new())
                                }
                                GeneratedContent::Reasoning(ref _s) => {
                                    GeneratedContent::Reasoning(String::new())
                                }
                                GeneratedContent::ToolCall(_) => unreachable!(),
                            };

                            let _ = task_state.output_tx.send(EngineReply::Content(content));
                            task_state.current_content.push(empty); // replace with an empty placeholder
                        }

                        // Pre-check: if the next decode will exceed context size
                        let n_ctx = self.llm_ctx().n_ctx() as usize;
                        let n_past = slots[i].n_past as usize + 1; // +1 for this new token
                        if n_past + 1 >= n_ctx {
                            // context will be full on next decode tick:
                            // 1. try to find an idle slot and free it.
                            // 2. if no lru slot found, shift the slot kv/cache to squeeze some space.
                            let mut lru: Option<usize> = None;
                            let mut lru_time = i64::MAX;
                            for (j, s) in slots.iter().enumerate() {
                                if matches!(s.state, SlotState::Idle)
                                    && !s.cached_tokens.is_empty()
                                    && s.mtime < lru_time
                                {
                                    debug_assert_ne!(i, j);
                                    lru = Some(j);
                                    lru_time = s.mtime;
                                }
                            }
                            if let Some(nth) = lru {
                                self.dump_slot_to_cache_ram(&mut slots[nth], true);
                            } else {
                                // No idle slot to swap: we have keep_tokens defined -
                                // let's shift the slot kv/cache to squeeze some space.
                                let keep_tokens = task_state.config.keep_tokens;
                                if keep_tokens > 0 {
                                    match self.context_shift(&mut slots[i], keep_tokens, 0) {
                                        Ok(new_n_past) => {
                                            log::debug!(
                                                "[context_shift] slot={} seq_id={} keep={} n_past={}->{}",
                                                slots[i].seq_id,
                                                slots[i].seq_id,
                                                keep_tokens,
                                                slots[i].n_past,
                                                new_n_past,
                                            );
                                            slots[i].n_past = new_n_past;
                                        }
                                        Err(e) => {
                                            log::error!(
                                                "[context_shift] failed for slot={}: {}",
                                                slots[i].seq_id,
                                                e,
                                            );
                                            slots[i].state = SlotState::Done {
                                                task_state,
                                                reply: EngineReply::Error(
                                                    "ContextFull".to_string(),
                                                ),
                                            };
                                            continue;
                                        }
                                    }
                                }
                            }
                        }

                        // decoding for sampled token
                        slots[i].state = SlotState::Decoding {
                            prompt_tokens: VecDeque::from([token]),
                            logits_for_all_tokens: false,
                            num_written: 0,
                            task_state,
                            is_prefill: false,
                        };
                    }
                    Err(e) => {
                        slots[i].state = SlotState::Done {
                            task_state,
                            reply: EngineReply::Error(e),
                        };
                    }
                }
            }
        }

        did_work || self.done_stage(slots) > 0
    }

    fn done_stage(&mut self, slots: &mut [Slot]) -> usize {
        let mut completed = 0usize;

        for slot in slots {
            let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

            if let SlotState::Done { task_state, reply } = state0 {
                if matches!(reply, EngineReply::Cancelled) {
                    log::info!("[worker] task id={} cancelled", task_state.config.task_id);
                }
                let aborted = matches!(
                    reply,
                    EngineReply::Cancelled | EngineReply::Shutdown | EngineReply::Error(_)
                );

                if task_state.config.output_stream {
                    // In continuous mode, content should have been streamed already
                    // Verify all elements are empty
                    debug_assert!(
                        task_state
                            .current_content
                            .iter()
                            .all(|c| c.as_str().is_empty()),
                        "Continuous mode: current_content should be empty after streaming, but found: {:?}",
                        task_state.current_content
                    );
                } else {
                    if !aborted {
                        // Send all accumulated content
                        for content in task_state.current_content {
                            if !content.as_str().is_empty() {
                                let _ = task_state.output_tx.send(EngineReply::Content(content));
                            }
                        }
                    }
                }

                let _ = task_state.output_tx.send(reply);

                if aborted {
                    self.clear_slot(slot);
                    slot.n_past = 0;
                } else {
                    self.reset_sampler(slot);
                }
                completed += 1;
                slot.state = SlotState::Idle;
            } else {
                slot.state = state0;
            }
        }

        completed
    }

    /// tick task pipeline
    fn tick(&mut self, slots: &mut [Slot], slot_cursor: &mut usize) -> bool {
        let has_sampling = self.decoding_stage(slots, slot_cursor);

        if has_sampling
            || slots
                .iter()
                .any(|s| matches!(s.state, SlotState::Sampling { .. }))
        {
            self.sampling_stage(slots)
        } else {
            self.done_stage(slots) > 0
        }
    }
}
