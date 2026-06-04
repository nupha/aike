use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crossbeam::channel::Receiver;

use super::{
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
    let mut slots = (0..max_parallel).map(Slot::new).collect::<Vec<_>>();
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
                } = std::mem::replace(&mut slot.state, SlotState::Busy)
                else {
                    unreachable!();
                };

                let n_tokens = prompt_tokens.len();
                let n_take = n_tokens.min(self.ubatch_size.saturating_sub(cnt));
                let mut feed_count = 0usize;
                let mut decode_err: Option<String> = None;

                for token in prompt_tokens.drain(..n_take) {
                    let need_logits = task_state.config.logits_for_all_tokens || n_take == n_tokens;
                    match self.push_decode_token(
                        slot,
                        token,
                        slot.n_past + feed_count as i32,
                        need_logits,
                    ) {
                        Ok(()) => {
                            num_written += 1;
                            feed_count += 1;
                            cnt += 1;
                        }
                        Err(e) => {
                            decode_err = Some(e);
                            break;
                        }
                    }
                }

                if let Some(e) = decode_err {
                    self.truncate_slot_cache(slot, slot.n_past as usize);
                    slot.state = SlotState::Done {
                        task_state,
                        reply: EngineReply::Error(e),
                    };
                    continue;
                }

                slot.state = if prompt_tokens.is_empty() {
                    // prefill done, go to sampling phase
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
                    }
                };
                if feed_count > 0 {
                    written_updates.push((slot_idx, feed_count));
                }
            }
        }

        if cnt > 0 {
            match self.decode_batch() {
                Ok(_) => {
                    for (idx, wrote) in &written_updates {
                        slots[*idx].n_past += *wrote as i32;
                    }
                }
                Err(e) => {
                    // Context shift is now handled proactively in sampling_stage
                    // before entering Decoding. If we still get a decode error here,
                    // it's a real error (not context full), so report it.
                    for (idx, _) in &written_updates {
                        let slot = &mut slots[*idx];
                        self.truncate_slot_cache(slot, slot.n_past as usize);
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

        for slot in slots.iter_mut() {
            let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

            if let SlotState::Sampling {
                logits,
                mut task_state,
            } = state0
            {
                if task_state.output_tx.is_full() {
                    // channel is full, wait sampling in next tick
                    slot.state = SlotState::Sampling { logits, task_state };
                } else if let Err(e) = self.init_sampler(
                    slot,
                    task_state.config.temperature,
                    task_state.config.top_p,
                    task_state.config.grammar.as_ref(),
                ) {
                    slot.state = SlotState::Done {
                        task_state,
                        reply: EngineReply::Error(e),
                    };
                } else {
                    did_work = true;

                    match self.sample(slot, logits) {
                        Ok(token) => {
                            self.sample_accept(slot, token);

                            slot.state = if model.is_eog(token) {
                                let n = task_state.n_completion_tokens;
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Stop(StopReason::Eog, n),
                                }
                            } else if task_state.config.max_tokens > 0
                                && task_state.n_completion_tokens >= task_state.config.max_tokens
                            {
                                let n = task_state.n_completion_tokens;
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Stop(StopReason::MaxTokens, n),
                                }
                            } else {
                                task_state.add_output_token(token, self.model());

                                #[cfg(debug_assertions)]
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

                                // Check stop words against the current content text
                                if let Some(w) = hit_stop_words(
                                    task_state.current_content.last().unwrap().as_str(),
                                    &task_state.config.stop_words,
                                ) {
                                    // Truncate stop word from tail of last content's text
                                    let last_content =
                                        task_state.current_content.last_mut().unwrap();

                                    let new_len =
                                        last_content.as_str().len().saturating_sub(w.len());

                                    last_content.truncate(new_len);

                                    let n = task_state.n_completion_tokens;
                                    SlotState::Done {
                                        task_state,
                                        reply: EngineReply::Stop(StopReason::Word(w), n),
                                    }
                                } else {
                                    // For streaming mode, send current content if not ToolCall
                                    if task_state.config.output_stream {
                                        debug_assert_eq!(
                                            task_state.current_content.len(),
                                            1,
                                            "continuous mode should have only 1 content element in sampling_stage"
                                        );

                                        let mut content = task_state.current_content.pop().unwrap();

                                        // Only send if not ToolCall
                                        if !matches!(content, GeneratedContent::ToolCall(_))
                                            && !content.as_str().is_empty()
                                        {
                                            let empty = match content {
                                                GeneratedContent::Text(_) => {
                                                    GeneratedContent::Text(String::new())
                                                }
                                                GeneratedContent::Reasoning(_) => {
                                                    GeneratedContent::Reasoning(String::new())
                                                }
                                                GeneratedContent::ToolCall(_) => {
                                                    GeneratedContent::ToolCall(String::new())
                                                }
                                            };

                                            let _ = task_state
                                                .output_tx
                                                .send(EngineReply::Content(content));

                                            content = empty;
                                        }

                                        task_state.current_content.push(content);
                                    }

                                    // Pre-check: if the next decode will exceed context,
                                    // perform context shift now before entering Decoding.
                                    let n_ctx = self.llm_ctx().n_ctx() as usize;
                                    let n_past = slot.n_past as usize + 1; // +1 for this new token
                                    let keep_tokens = task_state.config.keep_tokens;

                                    if n_past + 1 >= n_ctx && keep_tokens > 0 {
                                        // Context will be full on next decode, and we also defined keep_tokens -
                                        // let's shift the slot cache now to free some space.
                                        match self.context_shift(slot, keep_tokens, 0) {
                                            Ok(new_n_past) => {
                                                log::debug!(
                                                    "[context_shift] slot={} seq_id={} keep={} n_past={}->{}",
                                                    slot.seq_id,
                                                    slot.seq_id,
                                                    keep_tokens,
                                                    slot.n_past,
                                                    new_n_past,
                                                );
                                                slot.n_past = new_n_past;
                                            }
                                            Err(e) => {
                                                log::error!(
                                                    "[context_shift] failed for slot={}: {}",
                                                    slot.seq_id,
                                                    e,
                                                );
                                                slot.state = SlotState::Done {
                                                    task_state,
                                                    reply: EngineReply::Error(
                                                        "ContextFull".to_string(),
                                                    ),
                                                };
                                                continue;
                                            }
                                        }
                                    }

                                    SlotState::Decoding {
                                        prompt_tokens: VecDeque::from([token]),
                                        logits_for_all_tokens: false,
                                        num_written: 0,
                                        task_state,
                                    }
                                }
                            };
                        }
                        Err(e) => {
                            slot.state = SlotState::Done {
                                task_state,
                                reply: EngineReply::Error(e),
                            };
                        }
                    }
                }
            } else {
                slot.state = state0;
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
