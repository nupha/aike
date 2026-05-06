use std::{
    collections::{HashSet, VecDeque},
    sync::atomic::Ordering,
    thread,
    time::Duration,
};

use crossbeam::channel::Receiver;

use super::{
    slot::{Slot, SlotState},
    task::{EngineCtrlMsg, EngineReply, EngineTask, SchedResult},
};

/// Wrapper to send Engine pointer across thread boundaries.
#[repr(transparent)]
pub struct EnginePtr(pub *const super::context::Engine);
unsafe impl Send for EnginePtr {}

pub fn worker_loop(
    engine: EnginePtr,
    max_parallel: usize,
    idle_sleep: Duration,
    slot_match_threshold: f32,
    input_rx: Receiver<Box<EngineTask>>,
    ctrl_rx: Receiver<EngineCtrlMsg>,
) {
    // Raw pointer instead of Arc<Engine> to avoid reference cycle and self-join deadlock
    // (see comment at EnginePtr creation site for details).
    // SAFETY: engine pointer is valid for the worker's lifetime (Engine::drop joins the worker).
    let engine = unsafe { &*engine.0 };

    let mut slots = (0..max_parallel).map(Slot::new).collect::<Vec<_>>();
    let mut slot_cursor = 0usize;

    let mut ready_inputs = VecDeque::<Box<EngineTask>>::new();
    let mut deferred_inputs = VecDeque::<Box<EngineTask>>::new();

    let mut cancelled_ids = HashSet::<usize>::new();

    loop {
        if engine.handle_ctrl_messages(
            &ctrl_rx,
            &mut slots,
            &mut ready_inputs,
            &mut deferred_inputs,
            &mut cancelled_ids,
        ) {
            engine.num_tasks.store(0, Ordering::Relaxed);
            return;
        }

        while let Ok(input) = input_rx.try_recv() {
            if engine.handle_ctrl_messages(
                &ctrl_rx,
                &mut slots,
                &mut ready_inputs,
                &mut deferred_inputs,
                &mut cancelled_ids,
            ) {
                engine.num_tasks.store(0, Ordering::Relaxed);
                return;
            }
            enqueue_task(input, &mut ready_inputs, &mut deferred_inputs);
        }

        while let Some(task) = pop_next_task(&mut ready_inputs, &mut deferred_inputs) {
            if cancelled_ids.remove(&task.config.task_id) {
                let _ = task.output_tx.send(EngineReply::Cancelled);
            } else {
                match engine.sched_task(task, &mut slots, slot_match_threshold) {
                    SchedResult::Scheduled | SchedResult::Rejected => {}
                    SchedResult::Deferred(input) => {
                        enqueue_task(input, &mut ready_inputs, &mut deferred_inputs);
                        break;
                    }
                }
            }
        }

        let did_work = engine.tick(&mut slots, &mut slot_cursor);

        engine.num_tasks.store(
            slots
                .iter()
                .filter(|s| !matches!(s.state, SlotState::Idle))
                .count(),
            Ordering::Relaxed,
        );

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

pub fn remove_cancelled_inputs(queue: &mut VecDeque<Box<EngineTask>>, task_id: usize) {
    let mut kept = VecDeque::with_capacity(queue.len());
    while let Some(input) = queue.pop_front() {
        if input.config.task_id == task_id {
            let _ = input.output_tx.send(EngineReply::Cancelled);
        } else {
            kept.push_back(input);
        }
    }
    *queue = kept;
}

pub fn hit_stop_words(generated_text: &mut String, stop_words: &[String]) -> Option<String> {
    for stop in stop_words.iter().filter(|s| !s.is_empty()) {
        if generated_text.ends_with(stop) {
            return Some(stop.to_string());
        }
    }
    None
}
