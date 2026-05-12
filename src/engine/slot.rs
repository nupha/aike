use std::collections::VecDeque;

use crossbeam::channel::Sender;

use super::task::{EngineReply, TaskState, Token};

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
