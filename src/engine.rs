use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
    time::SystemTime,
};

use crossbeam::channel::{Receiver, SendError, Sender};
use llama_cpp_ffi::{LlamaContext, SamplerChain, SamplerKind};

pub type Token = i32;

#[derive(Debug, Clone)]
pub struct TaskConfig {
    /// Stable identifier used by control commands and client-side routing.
    pub task_id: String,
    /// Text-level stop conditions checked against generated text suffix.
    pub stop_words: Vec<String>,
    /// Maximum number of generated tokens; 0 means no explicit cap.
    pub max_tokens: usize,
    /// Sampling temperature for this task.
    pub temperature: f32,
    /// Top-p nucleus sampling threshold for this task.
    pub top_p: f32,
    /// Request logits on all prompt tokens (prefill-only style path).
    pub logits_for_all_tokens: bool,
    /// Output emission strategy (stream/single-step/buffered).
    pub output_mode: OutputMode,
}

pub struct EngineTask {
    /// Shared immutable config for task identity, sampling and stop behavior.
    pub config: TaskConfig,
    /// Prefilled prompt tokens to start decoding for this task.
    pub prompt_tokens: Vec<Token>,
    /// Optional preferred slot/sequence id for cache affinity.
    pub seq_hint: Option<usize>,
    /// Whether prompt prefix cache reuse is allowed for this task.
    pub use_cache: bool,
    /// Per-task output channel owned by the caller.
    pub output_tx: Sender<EngineReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Continuous,
    Single,
    OneShot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCtrlMsg {
    Shutdown,
    NextTokenOf(String),
    CancelTask(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Eog,
    MaxTokens,
    PrefillOnly,
    Word(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineReply {
    #[deprecated]
    Token(Token),
    #[deprecated]
    Tokens(Vec<Token>),
    Text(String),
    Stop(StopReason),
    Cancelled,
    Error(String),
    Shutdown,
}

pub trait EngineBackend: Send + 'static {
    /// Decode micro-batch capacity used by one llama_decode call.
    fn ubatch_size(&self) -> usize;
    /// Measure reusable prefix length between slot cache and new prompt.
    fn common_prefix_len(&self, seq_id: usize, prompt_tokens: &[Token]) -> usize;
    /// Align backend KV/cache to selected slot and return reused prefix length.
    fn prepare_slot(&mut self, seq_id: usize, prompt_tokens: &[Token]) -> Result<usize, String>;
    /// Roll back backend cache mirror when decode write fails.
    fn truncate_slot_cache(&mut self, seq_id: usize, n_keep: usize);
    /// Start assembling one shared decode batch for this tick.
    fn begin_decode_batch(&mut self);
    /// Push one token into decode batch for a slot at position `pos`.
    fn push_decode_token(
        &mut self,
        seq_id: usize,
        token: Token,
        pos: i32,
        need_logits: bool,
    ) -> Result<(), String>;
    /// Execute llama decode for all tokens pushed in this batch.
    fn decode_batch(&mut self) -> Result<(), String>;
    /// Sample next token from logits index produced by decode stage.
    fn sample(&mut self, seq_id: usize, logits_idx: i32) -> Result<Token, String>;
    /// Lazily initialize slot sampler from per-task sampling params.
    fn init_slot_sampler(
        &mut self,
        seq_id: usize,
        temperature: f32,
        top_p: f32,
    ) -> Result<(), String>;
    /// Convert token id into raw piece bytes for text-level stop matching.
    fn token_to_piece(&self, token: Token) -> Vec<u8>;
    /// Report whether token is model EOG/EOS.
    fn is_eog(&self, token: Token) -> bool;
    /// Clear slot runtime-only state while keeping reusable KV cache.
    fn reset_slot_runtime(&mut self, seq_id: usize);
    /// Hard-clear slot KV/cache/runtime state.
    fn clear_slot(&mut self, seq_id: usize);
}

pub struct LlamaEngineBackend {
    /// Shared llama context used by decode/sample stages.
    llm_ctx: LlamaContext,
    /// Effective max tokens per decode micro-batch.
    ubatch_size: usize,
    /// Per-slot mirror of tokens currently resident in KV cache.
    slot_cached_tokens: Vec<Vec<Token>>,
    /// Per-slot sampler lifecycle (create on demand, reset on completion).
    slot_samplers: Vec<Option<SamplerChain>>,
    /// Number of tokens queued in current decode batch.
    pending_decode_tokens: usize,
}

unsafe impl Send for LlamaEngineBackend {}

impl LlamaEngineBackend {
    pub fn new(mut llm_ctx: LlamaContext, max_parallel: usize) -> Self {
        let ubatch_size = (llm_ctx.n_ubatch() as usize).max(1);
        llm_ctx.init_batch(ubatch_size as i32, 0, max_parallel as i32);

        Self {
            llm_ctx,
            ubatch_size,
            slot_cached_tokens: vec![Vec::new(); max_parallel],
            slot_samplers: (0..max_parallel).map(|_| None).collect::<Vec<_>>(),
            pending_decode_tokens: 0,
        }
    }

    fn build_sampler_unchecked(&self, temperature: f32, top_p: f32) -> SamplerChain {
        let mut sampler = SamplerChain::new(self.llm_ctx.model().clone(), true);

        sampler
            .add_sampler(&SamplerKind::Penalties {
                repeat_last_n: 64,
                repeat_penalty: 1.1,
                frequency_penalty: 0.0,
                presence_penalty: 0.0,
            })
            .add_sampler(&SamplerKind::TopK(40))
            .add_sampler(&SamplerKind::TopP(top_p))
            .add_sampler(&SamplerKind::Temperature {
                temp: temperature,
                dyn_range: 0.0,
                dyn_exponent: 0.0,
            })
            .add_sampler(&SamplerKind::Dist(0));

        sampler
    }
}

impl EngineBackend for LlamaEngineBackend {
    fn ubatch_size(&self) -> usize {
        self.ubatch_size
    }

    fn common_prefix_len(&self, seq_id: usize, prompt_tokens: &[Token]) -> usize {
        let Some(cached) = self.slot_cached_tokens.get(seq_id) else {
            return 0;
        };
        let max_common = cached.len().min(prompt_tokens.len());
        let mut n_common = 0usize;
        while n_common < max_common && cached[n_common] == prompt_tokens[n_common] {
            n_common += 1;
        }
        n_common
    }

    /// Prepare one slot for a new task by aligning KV/cache state to prompt prefix reuse.
    ///
    /// Returns how many prompt tokens can be reused (`n_common`) so scheduler can skip them.
    /// This keeps backend KV cache and local mirror cache consistent before decode starts.
    fn prepare_slot(&mut self, seq_id: usize, prompt_tokens: &[Token]) -> Result<usize, String> {
        if seq_id >= self.slot_cached_tokens.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }

        let cached = &mut self.slot_cached_tokens[seq_id];
        let max_common = cached.len().min(prompt_tokens.len());
        let mut n_common = 0usize;
        while n_common < max_common && cached[n_common] == prompt_tokens[n_common] {
            n_common += 1;
        }

        if n_common == prompt_tokens.len() && n_common > 0 {
            // Full match still needs one decode token to refresh logits.
            n_common -= 1;
        }

        // Try truncating KV cache to the reusable prefix. If backend cannot truncate,
        // fall back to clearing the whole sequence to keep states consistent.
        let removed = self
            .llm_ctx
            .memory_seq_rm(seq_id as i32, n_common as i32, -1);
        if !removed {
            self.llm_ctx.memory_seq_rm(seq_id as i32, 0, -1);
            n_common = 0;
        }

        cached.truncate(n_common);
        Ok(n_common)
    }

    fn truncate_slot_cache(&mut self, seq_id: usize, n_keep: usize) {
        if let Some(cached) = self.slot_cached_tokens.get_mut(seq_id) {
            cached.truncate(n_keep);
        }
    }

    fn begin_decode_batch(&mut self) {
        self.pending_decode_tokens = 0;
    }

    fn push_decode_token(
        &mut self,
        seq_id: usize,
        token: Token,
        pos: i32,
        need_logits: bool,
    ) -> Result<(), String> {
        if seq_id >= self.slot_cached_tokens.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }
        if self.pending_decode_tokens >= self.ubatch_size {
            return Err("decode batch is full".to_string());
        }

        let idx = self.pending_decode_tokens;
        let batch = self.llm_ctx.batch_mut();
        unsafe {
            *batch.token.add(idx) = token;
            *batch.pos.add(idx) = pos;
            *(*batch.seq_id.add(idx)).add(0) = seq_id as i32;
            *batch.n_seq_id.add(idx) = 1;
            *batch.logits.add(idx) = if need_logits { 1 } else { 0 };
        }

        self.pending_decode_tokens += 1;
        self.slot_cached_tokens[seq_id].push(token);
        Ok(())
    }

    fn decode_batch(&mut self) -> Result<(), String> {
        if self.pending_decode_tokens == 0 {
            return Ok(());
        }
        let batch = self.llm_ctx.batch_mut();
        batch.n_tokens = self.pending_decode_tokens as i32;
        self.llm_ctx.decode()
    }

    fn sample(&mut self, seq_id: usize, logits_idx: i32) -> Result<Token, String> {
        if seq_id >= self.slot_samplers.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }
        let Some(sampler) = self.slot_samplers[seq_id].as_ref() else {
            return Err(format!("slot sampler is not ready: {}", seq_id));
        };
        Ok(sampler.sample_with_params(&self.llm_ctx, logits_idx, None))
    }

    fn init_slot_sampler(
        &mut self,
        seq_id: usize,
        temperature: f32,
        top_p: f32,
    ) -> Result<(), String> {
        if seq_id >= self.slot_samplers.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }
        if self.slot_samplers[seq_id].is_none() {
            self.slot_samplers[seq_id] = Some(self.build_sampler_unchecked(temperature, top_p));
        }
        Ok(())
    }

    fn token_to_piece(&self, token: Token) -> Vec<u8> {
        self.llm_ctx.model().token_to_piece(token)
    }

    fn is_eog(&self, token: Token) -> bool {
        self.llm_ctx.model().is_eog(token)
    }

    fn reset_slot_runtime(&mut self, seq_id: usize) {
        if seq_id < self.slot_samplers.len() {
            self.slot_samplers[seq_id] = None;
        }
    }

    fn clear_slot(&mut self, seq_id: usize) {
        if seq_id >= self.slot_cached_tokens.len() {
            return;
        }
        self.llm_ctx.memory_seq_rm(seq_id as i32, 0, -1);
        self.slot_cached_tokens[seq_id].clear();
        self.slot_samplers[seq_id] = None;
    }
}

#[derive(Debug)]
struct TaskState {
    config: TaskConfig,
    step_budget: usize,
    generated_tokens: Vec<Token>,
    generated_text: String,
    pending_bytes: Vec<u8>,
    output_tx: Sender<EngineReply>,
}

impl TaskState {
    fn append_utf8_piece(&mut self, piece: &[u8]) {
        if piece.is_empty() {
            return;
        }

        self.pending_bytes.extend_from_slice(piece);

        loop {
            match std::str::from_utf8(&self.pending_bytes) {
                Ok(s) => {
                    self.generated_text.push_str(s);
                    self.pending_bytes.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid == 0 {
                        break;
                    }

                    let s = unsafe { std::str::from_utf8_unchecked(&self.pending_bytes[..valid]) };
                    self.generated_text.push_str(s);
                    self.pending_bytes.drain(0..valid);

                    if self.pending_bytes.is_empty() {
                        break;
                    }
                }
            }
        }
    }

    fn append_token<B: EngineBackend>(&mut self, backend: &B, token: Token) {
        self.append_utf8_piece(&backend.token_to_piece(token));
    }
}

#[derive(Debug)]
enum SlotState {
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

struct Slot {
    seq_id: usize,
    n_past: i32,
    mtime: i64,
    state: SlotState,
}

impl Slot {
    fn new(seq_id: usize) -> Self {
        Self {
            seq_id,
            n_past: 0,
            mtime: 0,
            state: SlotState::Idle,
        }
    }

    fn output_tx(&self) -> Option<&Sender<EngineReply>> {
        match &self.state {
            SlotState::Decoding { task_state, .. } | SlotState::Sampling { task_state, .. } => {
                Some(&task_state.output_tx)
            }
            SlotState::Done { task_state, .. } => Some(&task_state.output_tx),
            SlotState::Idle | SlotState::Busy => None,
        }
    }
}

pub struct InferenceEngine {
    input_tx: Sender<EngineTask>,
    ctrl_tx: Sender<EngineCtrlMsg>,
    active_tasks: Arc<AtomicUsize>,
    max_parallel: usize,
    join_handle: Option<JoinHandle<()>>,
}

impl InferenceEngine {
    pub fn start(
        max_parallel: usize,
        idle_sleep: Duration,
        slot_match_threshold: f32,
        llm_ctx: LlamaContext,
    ) -> Self {
        Self::start_with_backend(
            max_parallel,
            idle_sleep,
            slot_match_threshold,
            LlamaEngineBackend::new(llm_ctx, max_parallel),
        )
    }

    pub fn start_with_backend<B: EngineBackend>(
        max_parallel: usize,
        idle_sleep: Duration,
        slot_match_threshold: f32,
        backend: B,
    ) -> Self {
        let (input_tx, input_rx) = crossbeam::channel::bounded::<EngineTask>(16);
        let (ctrl_tx, ctrl_rx) = crossbeam::channel::bounded::<EngineCtrlMsg>(1);

        let active_tasks = Arc::new(AtomicUsize::new(0));
        let worker_active = active_tasks.clone();

        let worker_join = thread::spawn(move || {
            worker_loop(
                max_parallel,
                idle_sleep,
                slot_match_threshold,
                backend,
                input_rx,
                ctrl_rx,
                worker_active,
            );
        });

        Self {
            input_tx,
            ctrl_tx,
            active_tasks,
            max_parallel,
            join_handle: Some(worker_join),
        }
    }

    pub fn push_task(
        &self,
        task_id: impl Into<String>,
        prompt_tokens: Vec<Token>,
        stop_words: Vec<String>,
        output_mode: OutputMode,
    ) -> Result<Receiver<EngineReply>, SendError<EngineTask>> {
        self.push_task_with_params(
            task_id,
            prompt_tokens,
            stop_words,
            output_mode,
            None,
            true,
            0,
            1.0,
            1.0,
            false,
        )
    }

    pub fn push_task_with_params(
        &self,
        task_id: impl Into<String>,
        prompt_tokens: Vec<Token>,
        stop_words: Vec<String>,
        output_mode: OutputMode,
        seq_hint: Option<usize>,
        use_cache: bool,
        max_tokens: usize,
        temperature: f32,
        top_p: f32,
        logits_for_all_tokens: bool,
    ) -> Result<Receiver<EngineReply>, SendError<EngineTask>> {
        let (output_tx, output_rx) = crossbeam::channel::bounded::<EngineReply>(1);

        self.input_tx.send(EngineTask {
            config: TaskConfig {
                task_id: task_id.into(),
                stop_words,
                max_tokens,
                temperature,
                top_p,
                logits_for_all_tokens,
                output_mode,
            },
            prompt_tokens,
            seq_hint,
            use_cache,
            output_tx,
        })?;
        Ok(output_rx)
    }

    pub fn next_token_of(
        &self,
        task_id: impl Into<String>,
    ) -> Result<(), SendError<EngineCtrlMsg>> {
        self.ctrl_tx
            .send(EngineCtrlMsg::NextTokenOf(task_id.into()))
    }

    pub fn cancel_task(&self, task_id: impl Into<String>) -> Result<(), SendError<EngineCtrlMsg>> {
        self.ctrl_tx.send(EngineCtrlMsg::CancelTask(task_id.into()))
    }

    pub fn active_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::Relaxed)
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    pub fn shutdown(mut self) {
        let _ = self.ctrl_tx.send(EngineCtrlMsg::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for InferenceEngine {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(EngineCtrlMsg::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

fn worker_loop<B: EngineBackend>(
    max_parallel: usize,
    idle_sleep: Duration,
    slot_match_threshold: f32,
    mut backend: B,
    input_rx: Receiver<EngineTask>,
    ctrl_rx: Receiver<EngineCtrlMsg>,
    active_counter: Arc<AtomicUsize>,
) {
    let mut slots = (0..max_parallel).map(Slot::new).collect::<Vec<_>>();
    let mut slot_cursor = 0usize;

    // Keep two queues so hinted tasks can be retried without starving normal tasks.
    let mut ready_inputs = VecDeque::<EngineTask>::new();
    let mut deferred_inputs = VecDeque::<EngineTask>::new();
    let mut cancelled_ids = HashSet::<String>::new();

    loop {
        // Control channel has the highest priority (shutdown/cancel/step).
        if handle_ctrl_messages(
            &ctrl_rx,
            &mut backend,
            &mut slots,
            &mut ready_inputs,
            &mut deferred_inputs,
            &mut cancelled_ids,
        ) {
            active_counter.store(0, Ordering::Relaxed);
            return;
        }

        while let Ok(input) = input_rx.try_recv() {
            if handle_ctrl_messages(
                &ctrl_rx,
                &mut backend,
                &mut slots,
                &mut ready_inputs,
                &mut deferred_inputs,
                &mut cancelled_ids,
            ) {
                active_counter.store(0, Ordering::Relaxed);
                return;
            }
            enqueue_task(input, &mut ready_inputs, &mut deferred_inputs);
        }

        while let Some(task) = pop_next_task(&mut ready_inputs, &mut deferred_inputs) {
            if cancelled_ids.remove(&task.config.task_id) {
                let _ = task.output_tx.send(EngineReply::Cancelled);
                continue;
            }

            match sched(task, slot_match_threshold, &mut backend, &mut slots) {
                SchedResult::Scheduled | SchedResult::Rejected => {}
                SchedResult::Deferred(input) => {
                    enqueue_task(input, &mut ready_inputs, &mut deferred_inputs);
                    break;
                }
            }
        }

        let did_work = tick(&mut backend, &mut slots, &mut slot_cursor);
        active_counter.store(count_active_tasks(&slots), Ordering::Relaxed);
        if !did_work {
            thread::sleep(idle_sleep);
        }
    }
}

fn enqueue_task(
    task: EngineTask,
    ready: &mut VecDeque<EngineTask>,
    deferred: &mut VecDeque<EngineTask>,
) {
    if task.seq_hint.unwrap_or(0) == 0 {
        ready.push_back(task);
    } else {
        deferred.push_back(task);
    }
}

fn pop_next_task(
    ready: &mut VecDeque<EngineTask>,
    deferred: &mut VecDeque<EngineTask>,
) -> Option<EngineTask> {
    if let Some(v) = ready.pop_front() {
        return Some(v);
    }
    if deferred.is_empty() {
        return None;
    }

    // Keep the same promotion behavior as llama-use1 worker queue.
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

fn tick<B: EngineBackend>(backend: &mut B, slots: &mut [Slot], slot_cursor: &mut usize) -> bool {
    let has_sampling = decoding_stage(backend, slots, slot_cursor);
    if has_sampling
        || slots
            .iter()
            .any(|s| matches!(s.state, SlotState::Sampling { .. }))
    {
        sampling_stage(backend, slots)
    } else {
        done_stage(backend, slots) > 0
    }
}

fn handle_ctrl_messages<B: EngineBackend>(
    ctrl_rx: &Receiver<EngineCtrlMsg>,
    backend: &mut B,
    slots: &mut [Slot],
    ready: &mut VecDeque<EngineTask>,
    deferred: &mut VecDeque<EngineTask>,
    cancelled_ids: &mut HashSet<String>,
) -> bool {
    while let Ok(ctrl) = ctrl_rx.try_recv() {
        match ctrl {
            EngineCtrlMsg::Shutdown => {
                for slot in slots.iter_mut() {
                    let state = std::mem::replace(&mut slot.state, SlotState::Busy);
                    if let SlotState::Decoding { task_state, .. }
                    | SlotState::Sampling { task_state, .. }
                    | SlotState::Done { task_state, .. } = state
                    {
                        let _ = task_state.output_tx.send(EngineReply::Shutdown);
                        backend.clear_slot(slot.seq_id);
                    }
                    slot.n_past = 0;
                    slot.state = SlotState::Idle;
                }
                return true;
            }
            EngineCtrlMsg::NextTokenOf(task_id) => {
                for slot in slots.iter_mut() {
                    let state = std::mem::replace(&mut slot.state, SlotState::Busy);
                    slot.state = match state {
                        SlotState::Decoding {
                            prompt_tokens,
                            logits_for_all_tokens,
                            num_written,
                            mut task_state,
                        } => {
                            if task_state.config.task_id == task_id
                                && task_state.config.output_mode == OutputMode::Single
                            {
                                task_state.step_budget = task_state.step_budget.saturating_add(1);
                            }
                            SlotState::Decoding {
                                prompt_tokens,
                                logits_for_all_tokens,
                                num_written,
                                task_state,
                            }
                        }
                        SlotState::Sampling {
                            logits,
                            mut task_state,
                        } => {
                            if task_state.config.task_id == task_id
                                && task_state.config.output_mode == OutputMode::Single
                            {
                                task_state.step_budget = task_state.step_budget.saturating_add(1);
                            }
                            SlotState::Sampling { logits, task_state }
                        }
                        other => other,
                    }
                }
            }
            EngineCtrlMsg::CancelTask(task_id) => {
                remove_cancelled_inputs(ready, &task_id);
                remove_cancelled_inputs(deferred, &task_id);
                cancelled_ids.insert(task_id.clone());

                for slot in slots.iter_mut() {
                    let state = std::mem::replace(&mut slot.state, SlotState::Busy);

                    slot.state = match state {
                        SlotState::Decoding {
                            prompt_tokens,
                            logits_for_all_tokens,
                            num_written,
                            task_state,
                        } => {
                            if task_state.config.task_id == task_id {
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Cancelled,
                                }
                            } else {
                                SlotState::Decoding {
                                    prompt_tokens,
                                    logits_for_all_tokens,
                                    num_written,
                                    task_state,
                                }
                            }
                        }
                        SlotState::Sampling { logits, task_state } => {
                            if task_state.config.task_id == task_id {
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Cancelled,
                                }
                            } else {
                                SlotState::Sampling { logits, task_state }
                            }
                        }
                        SlotState::Done { task_state, reply } => {
                            if task_state.config.task_id == task_id {
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Cancelled,
                                }
                            } else {
                                SlotState::Done { task_state, reply }
                            }
                        }
                        other => other,
                    };
                }
            }
        }
    }

    false
}

fn remove_cancelled_inputs(queue: &mut VecDeque<EngineTask>, task_id: &str) {
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

enum SchedResult {
    Scheduled,
    Deferred(EngineTask),
    Rejected,
}

fn sched<B: EngineBackend>(
    mut task: EngineTask,
    slot_match_threshold: f32,
    backend: &mut B,
    slots: &mut [Slot],
) -> SchedResult {
    if task.prompt_tokens.is_empty() {
        let _ = task.output_tx.send(EngineReply::Error(
            "prompt_tokens must not be empty".to_string(),
        ));
        return SchedResult::Rejected;
    }

    // Slot selection follows llama-use1 semantics:
    // seq_hint first, then prefix reuse threshold, then LRU fallback.
    let Some((slot_idx, allow_cache_reuse)) = pick_idle_slot(
        backend,
        slots,
        task.seq_hint,
        &task.prompt_tokens,
        task.use_cache,
        slot_match_threshold,
    ) else {
        return SchedResult::Deferred(task);
    };

    let seq_id = slots[slot_idx].seq_id;
    let n_common = if allow_cache_reuse {
        match backend.prepare_slot(seq_id, &task.prompt_tokens) {
            Ok(n) => n,
            Err(e) => {
                let _ = task.output_tx.send(EngineReply::Error(e));
                return SchedResult::Rejected;
            }
        }
    } else {
        backend.clear_slot(seq_id);
        backend.truncate_slot_cache(seq_id, 0);
        0
    };

    task.prompt_tokens.drain(..n_common);
    if task.prompt_tokens.is_empty() {
        let _ = task.output_tx.send(EngineReply::Error(
            "prompt_tokens became empty after prefix match".to_string(),
        ));
        return SchedResult::Rejected;
    }

    let config = task.config;
    let is_single_mode = config.output_mode == OutputMode::Single;

    let task_state = TaskState {
        config,
        step_budget: if is_single_mode { 1 } else { 0 },
        generated_tokens: Vec::new(),
        generated_text: String::new(),
        pending_bytes: Vec::new(),
        output_tx: task.output_tx,
    };

    let slot = &mut slots[slot_idx];
    slot.n_past = n_common as i32;
    slot.mtime = now_unix_s();
    slot.state = SlotState::Decoding {
        prompt_tokens: VecDeque::from(task.prompt_tokens),
        logits_for_all_tokens: task_state.config.logits_for_all_tokens,
        num_written: 0,
        task_state,
    };

    SchedResult::Scheduled
}

fn decoding_stage<B: EngineBackend>(
    backend: &mut B,
    slots: &mut [Slot],
    slot_cursor: &mut usize,
) -> bool {
    let mut has_sampling = false;
    let mut cnt = 0usize;
    let mut written_updates: Vec<(usize, usize)> = Vec::new();

    // Build one shared decode batch across slots, then decode once.
    backend.begin_decode_batch();

    let slots_len = slots.len();
    for offset in 0..slots_len {
        if cnt == backend.ubatch_size() {
            break;
        }

        let idx = (*slot_cursor + offset) % slots_len;
        let slot = &mut slots[idx];
        let state = std::mem::replace(&mut slot.state, SlotState::Busy);

        if let SlotState::Decoding {
            mut prompt_tokens,
            logits_for_all_tokens,
            mut num_written,
            task_state,
        } = state
        {
            let n_tokens = prompt_tokens.len();
            let n_take = n_tokens.min(backend.ubatch_size().saturating_sub(cnt));

            let mut feed_count = 0usize;
            let mut decode_err: Option<String> = None;

            for token in prompt_tokens.drain(..n_take) {
                // Request logits for all prompt tokens only when explicitly needed.
                let need_logits = task_state.config.logits_for_all_tokens || n_take == n_tokens;

                match backend.push_decode_token(
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
                backend.truncate_slot_cache(slot.seq_id, slot.n_past as usize);
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
                        reply: EngineReply::Stop(StopReason::PrefillOnly),
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
            slot.state = state;
        }
    }

    if cnt > 0 {
        match backend.decode_batch() {
            Ok(_) => {
                for (idx, wrote) in &written_updates {
                    slots[*idx].n_past += *wrote as i32;
                }
            }
            Err(e) => {
                for (idx, _) in &written_updates {
                    let slot = &mut slots[*idx];
                    backend.truncate_slot_cache(slot.seq_id, slot.n_past as usize);
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

fn sampling_stage<B: EngineBackend>(backend: &mut B, slots: &mut [Slot]) -> bool {
    let mut did_work = false;

    for slot in slots.iter_mut() {
        let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

        if let SlotState::Sampling {
            logits,
            mut task_state,
        } = state0
        {
            let can_sample = match task_state.config.output_mode {
                OutputMode::Continuous | OutputMode::OneShot => true,
                OutputMode::Single => task_state.step_budget > 0,
            } && !task_state.output_tx.is_full(); // if output_tx is full, don't sample

            if !can_sample {
                // restore task_state to perform sampling in next tick
                slot.state = SlotState::Sampling { logits, task_state };
            } else {
                if task_state.config.output_mode == OutputMode::Single {
                    task_state.step_budget = task_state.step_budget.saturating_sub(1);
                }

                if let Err(e) = backend.init_slot_sampler(
                    // lazy init sampler fail
                    slot.seq_id,
                    task_state.config.temperature,
                    task_state.config.top_p,
                ) {
                    slot.state = SlotState::Done {
                        task_state,
                        reply: EngineReply::Error(e),
                    };
                } else {
                    did_work = true;

                    match backend.sample(slot.seq_id, logits) {
                        Ok(token) => {
                            let mut out_text: String = "".into();

                            slot.state = if backend.is_eog(token) {
                                out_text = std::mem::replace(
                                    &mut task_state.generated_text,
                                    "".to_string(),
                                );
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Stop(StopReason::Eog),
                                }
                            } else if task_state.config.max_tokens > 0
                                && task_state.generated_tokens.len() >= task_state.config.max_tokens
                            {
                                out_text = std::mem::replace(
                                    &mut task_state.generated_text,
                                    "".to_string(),
                                );
                                SlotState::Done {
                                    task_state,
                                    reply: EngineReply::Stop(StopReason::MaxTokens),
                                }
                            } else {
                                // Append token to task state, try detokenize token to utf8 text
                                task_state.append_token(backend, token);

                                if let Some(w) = hit_stop_words(
                                    &mut task_state.generated_text,
                                    &task_state.config.stop_words,
                                ) {
                                    // Ending with stop word.
                                    // chop the ending stop word
                                    let new_len =
                                        task_state.generated_text.len().saturating_sub(w.len());
                                    task_state.generated_text.truncate(new_len);

                                    out_text = std::mem::replace(
                                        &mut task_state.generated_text,
                                        "".to_string(),
                                    );

                                    SlotState::Done {
                                        task_state,
                                        reply: EngineReply::Stop(StopReason::Word(w)),
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

                            debug_assert!(
                                matches!(
                                    slot.state,
                                    SlotState::Done { .. }
                                        | SlotState::Sampling { .. }
                                        | SlotState::Decoding { .. }
                                ),
                                "new state must be done, sampling, or decoding"
                            );

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
            }
        } else {
            slot.state = state0;
        }
    }

    did_work || done_stage(backend, slots) > 0
}

fn done_stage<B: EngineBackend>(backend: &mut B, slots: &mut [Slot]) -> usize {
    let mut completed = 0usize;

    for slot in slots.iter_mut() {
        let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);
        if let SlotState::Done { task_state, reply } = state0 {
            if !task_state.generated_text.is_empty() {
                let _ = task_state
                    .output_tx
                    .send(EngineReply::Text(task_state.generated_text));
            }

            let should_clear = matches!(
                reply,
                EngineReply::Cancelled | EngineReply::Shutdown | EngineReply::Error(_)
            );

            if task_state.config.output_mode != OutputMode::OneShot {
                let _ = task_state.output_tx.send(reply);
            }

            if should_clear {
                backend.clear_slot(slot.seq_id);
                slot.n_past = 0;
            } else {
                // Keep KV/cache for prompt reuse across future tasks.
                backend.reset_slot_runtime(slot.seq_id);
            }

            completed += 1;
            slot.state = SlotState::Idle;
        } else {
            slot.state = state0;
        }
    }

    completed
}

fn count_active_tasks(slots: &[Slot]) -> usize {
    slots
        .iter()
        .filter(|slot| !matches!(slot.state, SlotState::Idle))
        .count()
}

fn hit_stop_words(generated_text: &mut String, stop_words: &[String]) -> Option<String> {
    for stop in stop_words.iter().filter(|s| !s.is_empty()) {
        if generated_text.ends_with(stop) {
            return Some(stop.to_string());
        }
    }
    None
}

#[deprecated]
fn append_utf8_piece(out_text: &mut String, pending_bytes: &mut Vec<u8>, piece: &[u8]) {
    if piece.is_empty() {
        return;
    }
    pending_bytes.extend_from_slice(piece);
    loop {
        match std::str::from_utf8(pending_bytes) {
            Ok(s) => {
                out_text.push_str(s);
                pending_bytes.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid == 0 {
                    break;
                }
                let valid_bytes: Vec<u8> = pending_bytes[..valid].to_vec();
                let rest = pending_bytes[valid..].to_vec();
                let s = unsafe { std::str::from_utf8_unchecked(&valid_bytes) };
                out_text.push_str(s);
                *pending_bytes = rest;
                if pending_bytes.is_empty() {
                    break;
                }
            }
        }
    }
}

fn now_unix_s() -> i64 {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    }
}

fn pick_idle_slot<B: EngineBackend>(
    backend: &B,
    slots: &[Slot],
    seq_hint: Option<usize>,
    prompt_tokens: &[Token],
    use_cache: bool,
    match_threshold: f32,
) -> Option<(usize, bool)> {
    if let Some(hint) = seq_hint
        && let Some(slot) = slots.get(hint)
        && matches!(slot.state, SlotState::Idle)
    {
        let can_reuse = use_cache && backend.common_prefix_len(slot.seq_id, prompt_tokens) > 0;
        return Some((hint, can_reuse));
    }

    let idle_indices = slots
        .iter()
        .enumerate()
        .filter_map(|(idx, s)| matches!(s.state, SlotState::Idle).then_some(idx))
        .collect::<Vec<_>>();
    if idle_indices.is_empty() {
        return None;
    }

    let mut best_idx: Option<usize> = None;
    let mut best_common = 0usize;
    if use_cache {
        for idx in &idle_indices {
            let common = backend.common_prefix_len(slots[*idx].seq_id, prompt_tokens);
            if common > best_common {
                best_common = common;
                best_idx = Some(*idx);
            }
        }
    }

    if use_cache && let Some(idx) = best_idx {
        let ratio = best_common as f32 / prompt_tokens.len().max(1) as f32;
        if ratio >= match_threshold {
            return Some((idx, true));
        }
    }

    let mut lru_idx = idle_indices[0];
    let mut lru_mtime = slots[lru_idx].mtime;
    for idx in idle_indices.iter().skip(1) {
        if slots[*idx].mtime < lru_mtime {
            lru_mtime = slots[*idx].mtime;
            lru_idx = *idx;
        }
    }
    Some((lru_idx, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct MockSlotPlan {
        outputs: Vec<Token>,
    }

    struct MockSlot {
        cached_tokens: Vec<Token>,
        outputs: Vec<Token>,
        sample_idx: usize,
    }

    struct MockEngine {
        slots: Vec<MockSlot>,
        eog_token: Option<Token>,
        n_ubatch: usize,
        pending: usize,
        decode_logits_owner: Vec<usize>,
        last_logits_owner: Vec<usize>,
    }

    impl MockEngine {
        fn new(plans: Vec<MockSlotPlan>, eog_token: Option<Token>, n_ubatch: usize) -> Self {
            let slots = plans
                .into_iter()
                .map(|p| MockSlot {
                    cached_tokens: Vec::new(),
                    outputs: p.outputs,
                    sample_idx: 0,
                })
                .collect::<Vec<_>>();
            Self {
                slots,
                eog_token,
                n_ubatch,
                pending: 0,
                decode_logits_owner: Vec::new(),
                last_logits_owner: Vec::new(),
            }
        }
    }

    impl EngineBackend for MockEngine {
        fn ubatch_size(&self) -> usize {
            self.n_ubatch
        }

        fn common_prefix_len(&self, seq_id: usize, prompt_tokens: &[Token]) -> usize {
            let Some(slot) = self.slots.get(seq_id) else {
                return 0;
            };
            let max_common = slot.cached_tokens.len().min(prompt_tokens.len());
            let mut n_common = 0usize;
            while n_common < max_common && slot.cached_tokens[n_common] == prompt_tokens[n_common] {
                n_common += 1;
            }
            n_common
        }

        fn prepare_slot(
            &mut self,
            seq_id: usize,
            prompt_tokens: &[Token],
        ) -> Result<usize, String> {
            let slot = self
                .slots
                .get_mut(seq_id)
                .ok_or_else(|| format!("invalid seq id {}", seq_id))?;
            let max_common = slot.cached_tokens.len().min(prompt_tokens.len());
            let mut n_common = 0usize;
            while n_common < max_common && slot.cached_tokens[n_common] == prompt_tokens[n_common] {
                n_common += 1;
            }
            if n_common == prompt_tokens.len() && n_common > 0 {
                n_common -= 1;
            }
            slot.cached_tokens.truncate(n_common);
            Ok(n_common)
        }

        fn truncate_slot_cache(&mut self, seq_id: usize, n_keep: usize) {
            if let Some(slot) = self.slots.get_mut(seq_id) {
                slot.cached_tokens.truncate(n_keep);
            }
        }

        fn begin_decode_batch(&mut self) {
            self.pending = 0;
            self.decode_logits_owner.clear();
        }

        fn push_decode_token(
            &mut self,
            seq_id: usize,
            token: Token,
            _pos: i32,
            _need_logits: bool,
        ) -> Result<(), String> {
            if self.pending >= self.n_ubatch {
                return Err("mock batch full".to_string());
            }
            let slot = self
                .slots
                .get_mut(seq_id)
                .ok_or_else(|| format!("invalid seq id {}", seq_id))?;
            slot.cached_tokens.push(token);
            self.decode_logits_owner.push(seq_id);
            self.pending += 1;
            Ok(())
        }

        fn decode_batch(&mut self) -> Result<(), String> {
            self.last_logits_owner = self.decode_logits_owner.clone();
            Ok(())
        }

        fn sample(&mut self, _seq_id: usize, logits_idx: i32) -> Result<Token, String> {
            let owner = *self
                .last_logits_owner
                .get(logits_idx as usize)
                .ok_or_else(|| "invalid logits idx".to_string())?;
            let slot = self
                .slots
                .get_mut(owner)
                .ok_or_else(|| "invalid logits owner".to_string())?;
            if slot.sample_idx >= slot.outputs.len() {
                return Err("no more scripted outputs".to_string());
            }
            let token = slot.outputs[slot.sample_idx];
            slot.sample_idx += 1;
            Ok(token)
        }

        fn is_eog(&self, token: Token) -> bool {
            self.eog_token == Some(token)
        }

        fn init_slot_sampler(
            &mut self,
            _seq_id: usize,
            _temperature: f32,
            _top_p: f32,
        ) -> Result<(), String> {
            Ok(())
        }

        fn token_to_piece(&self, token: Token) -> Vec<u8> {
            token.to_string().into_bytes()
        }

        fn reset_slot_runtime(&mut self, seq_id: usize) {
            if let Some(slot) = self.slots.get_mut(seq_id) {
                slot.sample_idx = 0;
            }
        }

        fn clear_slot(&mut self, seq_id: usize) {
            if let Some(slot) = self.slots.get_mut(seq_id) {
                slot.cached_tokens.clear();
                slot.sample_idx = 0;
            }
        }
    }

    fn recv_until_finished(rx: &Receiver<EngineReply>, timeout: Duration) -> Vec<EngineReply> {
        let mut out = Vec::new();
        for _ in 0..200 {
            match rx.recv_timeout(timeout) {
                Ok(msg) => {
                    let done = !matches!(msg, EngineReply::Token(_) | EngineReply::Tokens(_));
                    out.push(msg);
                    if done {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn engine_prefill_decode_sample_until_eog() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![101, 102],
            }],
            Some(102),
            8,
        );
        let handle = InferenceEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("t1", vec![1, 2, 3], Vec::new(), OutputMode::Continuous)
            .expect("submit t1");

        let out = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(out.iter().any(|m| matches!(m, EngineReply::Token(101))));
        assert!(out.iter().any(|m| matches!(m, EngineReply::Token(102))));
        assert!(
            out.iter()
                .any(|m| matches!(m, EngineReply::Stop(StopReason::Eog)))
        );
        handle.shutdown();
    }

    #[test]
    fn single_mode_needs_next_token() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![1, 2, 3],
            }],
            Some(3),
            8,
        );
        let handle = InferenceEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("s1", vec![9], Vec::new(), OutputMode::Single)
            .expect("submit s1");

        let first = recv_until_finished(&rx, Duration::from_millis(50));
        assert!(first.iter().any(|m| matches!(m, EngineReply::Token(1))));
        assert!(!first.iter().any(|m| matches!(m, EngineReply::Token(2))));

        handle.next_token_of("s1").expect("next token 2");
        let second = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(second.iter().any(|m| matches!(m, EngineReply::Token(2))));

        handle.next_token_of("s1").expect("next token 3");
        let third = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(third.iter().any(|m| matches!(m, EngineReply::Token(3))));
        assert!(
            third
                .iter()
                .any(|m| matches!(m, EngineReply::Stop(StopReason::Eog)))
        );
        handle.shutdown();
    }

    #[test]
    fn one_shot_outputs_tokens_once() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![11, 12, 13],
            }],
            Some(13),
            8,
        );
        let handle = InferenceEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("o1", vec![5], Vec::new(), OutputMode::OneShot)
            .expect("submit o1");

        let out = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(!out.iter().any(|m| matches!(m, EngineReply::Token(_))));
        assert!(out.iter().any(|m| matches!(
            m,
            EngineReply::Tokens(tokens) if tokens == &vec![11, 12, 13]
        )));
        assert!(
            out.iter()
                .any(|m| matches!(m, EngineReply::Stop(StopReason::Eog)))
        );
        handle.shutdown();
    }

    #[test]
    fn pick_idle_slot_respects_threshold_and_lru_fallback() {
        let engine = MockEngine::new(
            vec![
                MockSlotPlan { outputs: vec![1] },
                MockSlotPlan { outputs: vec![2] },
            ],
            None,
            8,
        );
        let mut slots = vec![Slot::new(0), Slot::new(1)];
        slots[0].mtime = 20;
        slots[1].mtime = 10;

        // prime cache mirrors for prefix match measurement
        let mut engine = engine;
        engine.slots[0].cached_tokens = vec![1, 2, 3];
        engine.slots[1].cached_tokens = vec![9];

        // common=3/5=0.6 < 0.9, should fall back to LRU slot(1) with no cache reuse
        let picked =
            pick_idle_slot(&engine, &slots, None, &[1, 2, 3, 4, 5], true, 0.9).expect("pick slot");
        assert_eq!(picked, (1, false));
    }

    #[test]
    fn pick_idle_slot_prefers_seq_hint_when_idle() {
        let engine = MockEngine::new(
            vec![
                MockSlotPlan { outputs: vec![1] },
                MockSlotPlan { outputs: vec![2] },
            ],
            None,
            8,
        );
        let slots = vec![Slot::new(0), Slot::new(1)];
        let picked = pick_idle_slot(&engine, &slots, Some(1), &[42], true, 0.9).expect("pick");
        assert_eq!(picked.0, 1);
    }

    #[test]
    fn cancel_task_stops_running_job() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![10, 11, 12, 13, 14],
            }],
            None,
            8,
        );
        let handle = InferenceEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("c1", vec![7], Vec::new(), OutputMode::Single)
            .expect("submit c1");

        // wait at least one output to ensure task is running
        let _ = rx.recv_timeout(Duration::from_millis(100));
        handle.cancel_task("c1").expect("cancel c1");

        let out = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(out.iter().any(|m| matches!(m, EngineReply::Cancelled)));
        handle.shutdown();
    }

    #[test]
    fn task_is_queued_when_no_idle_slot_then_runs() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![1, 2, 3, 4],
            }],
            Some(2),
            8,
        );
        let handle = InferenceEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);

        let rx1 = handle
            .push_task("q1", vec![10], Vec::new(), OutputMode::Continuous)
            .expect("submit q1");
        let rx2 = handle
            .push_task("q2", vec![20], Vec::new(), OutputMode::Continuous)
            .expect("submit q2");

        let out1 = recv_until_finished(&rx1, Duration::from_millis(100));
        assert!(
            out1.iter()
                .any(|m| matches!(m, EngineReply::Stop(StopReason::Eog)))
        );

        let out2 = recv_until_finished(&rx2, Duration::from_millis(100));
        assert!(
            out2.iter()
                .any(|m| matches!(m, EngineReply::Stop(StopReason::Eog)))
        );
        handle.shutdown();
    }

    #[test]
    fn cancel_task_can_remove_queued_job() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![1, 2, 3, 4, 5, 6],
            }],
            Some(3),
            8,
        );
        let handle = InferenceEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);

        let rx1 = handle
            .push_task("busy", vec![10], Vec::new(), OutputMode::Continuous)
            .expect("submit busy");
        let rx2 = handle
            .push_task("queued", vec![20], Vec::new(), OutputMode::Continuous)
            .expect("submit queued");

        let _ = rx1.recv_timeout(Duration::from_millis(100));
        handle.cancel_task("queued").expect("cancel queued");

        let out2 = recv_until_finished(&rx2, Duration::from_millis(100));
        assert!(out2.iter().any(|m| matches!(m, EngineReply::Cancelled)));
        handle.shutdown();
    }
}
