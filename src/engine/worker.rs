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
    task::{EngineCtrlMsg, EngineReply, EngineTask, OutputMode, SchedResult, StopReason},
};

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

    let mut ready_inputs = VecDeque::<Box<EngineTask>>::new();
    let mut deferred_inputs = VecDeque::<Box<EngineTask>>::new();

    let mut cancelled_ids = HashSet::<usize>::new();

    loop {
        if engine.lock().unwrap().handle_ctrl_messages(
            &ctrl_rx,
            &mut slots,
            &mut ready_inputs,
            &mut deferred_inputs,
            &mut cancelled_ids,
        ) {
            engine.lock().unwrap().num_tasks = 0;
            return;
        }

        while let Ok(input) = input_rx.try_recv() {
            if engine.lock().unwrap().handle_ctrl_messages(
                &ctrl_rx,
                &mut slots,
                &mut ready_inputs,
                &mut deferred_inputs,
                &mut cancelled_ids,
            ) {
                engine.lock().unwrap().num_tasks = 0;
                return;
            }
            enqueue_task(input, &mut ready_inputs, &mut deferred_inputs);
        }

        while let Some(task) = pop_next_task(&mut ready_inputs, &mut deferred_inputs) {
            if cancelled_ids.remove(&task.config.task_id) {
                let _ = task.output_tx.send(EngineReply::Cancelled);
            } else {
                match engine
                    .lock()
                    .unwrap()
                    .sched_task(task, &mut slots, slot_match_threshold)
                {
                    SchedResult::Scheduled | SchedResult::Rejected => {}
                    SchedResult::Deferred(input) => {
                        enqueue_task(input, &mut ready_inputs, &mut deferred_inputs);
                        break;
                    }
                }
            }
        }

        let did_work = engine.lock().unwrap().tick(&mut slots, &mut slot_cursor);

        engine.lock().unwrap().num_tasks = slots
            .iter()
            .filter(|s| !matches!(s.state, SlotState::Idle))
            .count();

        if !did_work {
            thread::sleep(idle_sleep);
        }
    }
}

pub fn enqueue_task(
    task: Box<EngineTask>,
    ready: &mut VecDeque<Box<EngineTask>>,
    deferred: &mut VecDeque<Box<EngineTask>>,
) {
    if task.seq_hint.unwrap_or(0) == 0 {
        ready.push_back(task);
    } else {
        deferred.push_back(task);
    }
}

pub fn pop_next_task(
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
        if deferred[i].seq_hint.unwrap_or(0) == 0
            && let Some(v) = deferred.remove(i)
        {
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

        self.begin_decode_batch();

        let slots_len = slots.len();
        for offset in 0..slots_len {
            if cnt == self.ubatch_size {
                break;
            }

            let idx = (*slot_cursor + offset) % slots_len;
            let slot = &mut slots[idx];
            let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

            if let SlotState::Decoding {
                mut prompt_tokens,
                logits_for_all_tokens,
                mut num_written,
                task_state,
            } = state0
            {
                let n_tokens = prompt_tokens.len();
                let n_take = n_tokens.min(self.ubatch_size.saturating_sub(cnt));
                let mut feed_count = 0usize;
                let mut decode_err: Option<String> = None;
                for token in prompt_tokens.drain(..n_take) {
                    let need_logits = task_state.config.logits_for_all_tokens || n_take == n_tokens;
                    match self.push_decode_token(
                        slot.seq_id,
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
                    self.truncate_slot_cache(slot.seq_id, slot.n_past as usize);
                    slot.state = SlotState::Done {
                        task_state,
                        reply: EngineReply::Error(e),
                    };
                    continue;
                }
                slot.state = if prompt_tokens.is_empty() {
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
                    SlotState::Decoding {
                        prompt_tokens,
                        logits_for_all_tokens,
                        num_written,
                        task_state,
                    }
                };
                if feed_count > 0 {
                    written_updates.push((idx, feed_count));
                }
            } else {
                slot.state = state0;
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
                    for (idx, _) in &written_updates {
                        let slot = &mut slots[*idx];
                        self.truncate_slot_cache(slot.seq_id, slot.n_past as usize);
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

        for slot in slots.iter_mut() {
            let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

            if let SlotState::Sampling {
                logits,
                mut task_state,
            } = state0
            {
                if task_state.output_tx.is_full() {
                    // do sampling in next tick
                    slot.state = SlotState::Sampling { logits, task_state };
                } else if let Err(e) = self.init_slot_sampler(
                    slot.seq_id,
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

                    match self.sample(slot.seq_id, logits) {
                        Ok(token) => {
                            self.accept_token(slot.seq_id, token);

                            let mut out_text: String = "".into();
                            slot.state = if self.is_eog(token) {
                                out_text = std::mem::replace(
                                    &mut task_state.generated_text,
                                    "".to_string(),
                                );
                                let n = task_state.n_completion_tokens;
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Stop(StopReason::Eog, n),
                                }
                            } else if task_state.config.max_tokens > 0
                                && task_state.n_completion_tokens >= task_state.config.max_tokens
                            {
                                out_text = std::mem::replace(
                                    &mut task_state.generated_text,
                                    "".to_string(),
                                );
                                let n = task_state.n_completion_tokens;
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Stop(StopReason::MaxTokens, n),
                                }
                            } else {
                                self.append_token(&mut task_state, token);

                                if let Some(w) = hit_stop_words(
                                    &task_state.generated_text,
                                    &task_state.config.stop_words,
                                ) {
                                    let new_len =
                                        task_state.generated_text.len().saturating_sub(w.len());
                                    task_state.generated_text.truncate(new_len);

                                    out_text = std::mem::replace(
                                        &mut task_state.generated_text,
                                        "".to_string(),
                                    );
                                    let n = task_state.n_completion_tokens;
                                    SlotState::Done {
                                        task_state,
                                        reply: EngineReply::Stop(StopReason::Word(w), n),
                                    }
                                } else {
                                    if task_state.config.output_mode != OutputMode::OneShot {
                                        out_text = std::mem::replace(
                                            &mut task_state.generated_text,
                                            "".to_string(),
                                        );
                                    }
                                    SlotState::Decoding {
                                        prompt_tokens: VecDeque::from([token]),
                                        logits_for_all_tokens: false,
                                        num_written: 0,
                                        task_state,
                                    }
                                }
                            };

                            if !out_text.is_empty()
                                && let Some(tx) = slot.output_tx()
                            {
                                let _ = tx.send(EngineReply::Text(out_text));
                            }
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
                let aborted = matches!(
                    reply,
                    EngineReply::Cancelled | EngineReply::Shutdown | EngineReply::Error(_)
                );
                match task_state.config.output_mode {
                    OutputMode::OneShot => {
                        if !aborted {
                            let _ = task_state
                                .output_tx
                                .send(EngineReply::Text(task_state.generated_text));
                        }
                    }
                    OutputMode::Continuous => {
                        debug_assert!(task_state.generated_text.is_empty());
                    }
                }
                let _ = task_state.output_tx.send(reply);
                if aborted {
                    self.clear_slot(slot.seq_id);
                    slot.n_past = 0;
                } else {
                    self.reset_slot_runtime(slot.seq_id);
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
