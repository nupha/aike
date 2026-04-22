use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
    time::SystemTime,
};

use llama_cpp_ffi::{LlamaContext, SamplerChain, SamplerKind};

pub type Token = i32;

#[derive(Clone)]
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

pub struct PipelineTask {
    /// Shared immutable config for task identity, sampling and stop behavior.
    pub config: TaskConfig,
    /// Prefilled prompt tokens to start decoding for this task.
    pub prompt_tokens: Vec<Token>,
    /// Optional preferred slot/sequence id for cache affinity.
    pub seq_hint: Option<usize>,
    /// Whether prompt prefix cache reuse is allowed for this task.
    pub use_cache: bool,
    /// Per-task output channel owned by the caller.
    pub output_tx: mpsc::Sender<PipelineReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Continuous,
    Single,
    OneShot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineCtrlMsg {
    Shutdown,
    NextTokenOf(String),
    CancelTask(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Eog,
    Length,
    PrefillOnly,
    StopSequence,
    Shutdown,
    Cancelled,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineReply {
    Token(Token),
    Tokens(Vec<Token>),
    Finished(FinishReason),
}

pub trait PipelineBackend: Send + 'static {
    /// Decode micro-batch capacity used by one llama_decode call.
    fn n_ubatch(&self) -> usize;
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
    fn ensure_slot_sampler(
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

pub struct PipelineLlamaBackend {
    /// Shared llama context used by decode/sample stages.
    llm_ctx: LlamaContext,
    /// Cached model EOG token id for fast stop checks.
    eog_token: Token,
    /// Effective max tokens per decode micro-batch.
    n_ubatch: usize,
    /// Per-slot mirror of tokens currently resident in KV cache.
    slot_cached_tokens: Vec<Vec<Token>>,
    /// Per-slot sampler lifecycle (create on demand, reset on completion).
    slot_samplers: Vec<Option<SamplerChain<'static>>>,
    /// Number of tokens queued in current decode batch.
    pending_decode_tokens: usize,
}

unsafe impl Send for PipelineLlamaBackend {}

impl PipelineLlamaBackend {
    pub fn new(mut llm_ctx: LlamaContext, max_parallel: usize) -> Self {
        let n_ubatch = (llm_ctx.n_ubatch() as usize).max(1);
        llm_ctx.init_batch(n_ubatch as i32, 0, max_parallel as i32);
        let eog_token = llm_ctx.model().eog_token_id();

        Self {
            llm_ctx,
            eog_token,
            n_ubatch,
            slot_cached_tokens: vec![Vec::new(); max_parallel],
            slot_samplers: (0..max_parallel).map(|_| None).collect::<Vec<_>>(),
            pending_decode_tokens: 0,
        }
    }

    fn build_sampler_unchecked(&self, temperature: f32, top_p: f32) -> SamplerChain<'static> {
        let sampler = SamplerChain::new(self.llm_ctx.model(), true);
        sampler.add_sampler(&SamplerKind::Penalties {
            repeat_last_n: 64,
            repeat_penalty: 1.1,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
        });
        sampler.add_sampler(&SamplerKind::TopK(40));
        sampler.add_sampler(&SamplerKind::TopP(top_p));
        sampler.add_sampler(&SamplerKind::Temperature {
            temp: temperature,
            dyn_range: 0.0,
            dyn_exponent: 0.0,
        });
        sampler.add_sampler(&SamplerKind::Dist(0));
        // Safety: backend owns a 'static context/model for the full worker lifetime.
        unsafe { std::mem::transmute::<SamplerChain<'_>, SamplerChain<'static>>(sampler) }
    }
}

impl PipelineBackend for PipelineLlamaBackend {
    fn n_ubatch(&self) -> usize {
        self.n_ubatch
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
        if self.pending_decode_tokens >= self.n_ubatch {
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

    fn ensure_slot_sampler(
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
        self.eog_token == token
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

struct TaskState {
    config: TaskConfig,
    step_budget: usize,
    generated_tokens: Vec<Token>,
    generated_text: String,
    pending_bytes: Vec<u8>,
    output_tx: mpsc::Sender<PipelineReply>,
}

enum SlotState {
    Idle,
    Decoding {
        prompt_tokens: VecDeque<Token>,
        logits_for_all_tokens: bool,
        num_written: usize,
        task: TaskState,
    },
    Sampling {
        logits: i32,
        task: TaskState,
    },
    Done {
        task: TaskState,
        reason: FinishReason,
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
}

pub struct PipelineEngine {
    input_tx: mpsc::Sender<PipelineTask>,
    ctrl_tx: mpsc::Sender<PipelineCtrlMsg>,
    active_tasks: Arc<AtomicUsize>,
    max_parallel: usize,
    join_handle: Option<JoinHandle<()>>,
}

impl PipelineEngine {
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
            PipelineLlamaBackend::new(llm_ctx, max_parallel),
        )
    }

    pub fn start_with_backend<B: PipelineBackend>(
        max_parallel: usize,
        idle_sleep: Duration,
        slot_match_threshold: f32,
        backend: B,
    ) -> Self {
        let (input_tx, input_rx) = mpsc::channel::<PipelineTask>();
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<PipelineCtrlMsg>();
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
    ) -> Result<mpsc::Receiver<PipelineReply>, mpsc::SendError<PipelineTask>> {
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
    ) -> Result<mpsc::Receiver<PipelineReply>, mpsc::SendError<PipelineTask>> {
        let (output_tx, output_rx) = mpsc::channel::<PipelineReply>();
        self.input_tx.send(PipelineTask {
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
    ) -> Result<(), mpsc::SendError<PipelineCtrlMsg>> {
        self.ctrl_tx
            .send(PipelineCtrlMsg::NextTokenOf(task_id.into()))
    }

    pub fn cancel_task(
        &self,
        task_id: impl Into<String>,
    ) -> Result<(), mpsc::SendError<PipelineCtrlMsg>> {
        self.ctrl_tx
            .send(PipelineCtrlMsg::CancelTask(task_id.into()))
    }

    pub fn active_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::Relaxed)
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    pub fn shutdown(mut self) {
        let _ = self.ctrl_tx.send(PipelineCtrlMsg::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for PipelineEngine {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(PipelineCtrlMsg::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

fn worker_loop<B: PipelineBackend>(
    max_parallel: usize,
    idle_sleep: Duration,
    slot_match_threshold: f32,
    mut backend: B,
    input_rx: mpsc::Receiver<PipelineTask>,
    ctrl_rx: mpsc::Receiver<PipelineCtrlMsg>,
    active_counter: Arc<AtomicUsize>,
) {
    let mut slots = (0..max_parallel).map(Slot::new).collect::<Vec<_>>();
    let mut slot_cursor = 0usize;
    // Keep two queues so hinted tasks can be retried without starving normal tasks.
    let mut ready_inputs = VecDeque::<PipelineTask>::new();
    let mut deferred_inputs = VecDeque::<PipelineTask>::new();
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
            enqueue_input(input, &mut ready_inputs, &mut deferred_inputs);
        }

        while let Some(input) = pop_next_input(&mut ready_inputs, &mut deferred_inputs) {
            if cancelled_ids.remove(&input.config.task_id) {
                let _ = input
                    .output_tx
                    .send(PipelineReply::Finished(FinishReason::Cancelled));
                continue;
            }

            match sched(
                input,
                max_parallel,
                slot_match_threshold,
                &mut backend,
                &mut slots,
            ) {
                SchedResult::Scheduled | SchedResult::Rejected => {}
                SchedResult::Deferred(input) => {
                    enqueue_input(input, &mut ready_inputs, &mut deferred_inputs);
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

fn enqueue_input(
    input: PipelineTask,
    ready: &mut VecDeque<PipelineTask>,
    deferred: &mut VecDeque<PipelineTask>,
) {
    if input.seq_hint.unwrap_or(0) == 0 {
        ready.push_back(input);
    } else {
        deferred.push_back(input);
    }
}

fn pop_next_input(
    ready: &mut VecDeque<PipelineTask>,
    deferred: &mut VecDeque<PipelineTask>,
) -> Option<PipelineTask> {
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

fn tick<B: PipelineBackend>(backend: &mut B, slots: &mut [Slot], slot_cursor: &mut usize) -> bool {
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

fn handle_ctrl_messages<B: PipelineBackend>(
    ctrl_rx: &mpsc::Receiver<PipelineCtrlMsg>,
    backend: &mut B,
    slots: &mut [Slot],
    ready: &mut VecDeque<PipelineTask>,
    deferred: &mut VecDeque<PipelineTask>,
    cancelled_ids: &mut HashSet<String>,
) -> bool {
    while let Ok(ctrl) = ctrl_rx.try_recv() {
        match ctrl {
            PipelineCtrlMsg::Shutdown => {
                for slot in slots.iter_mut() {
                    let state = std::mem::replace(&mut slot.state, SlotState::Busy);
                    if let SlotState::Decoding { task, .. }
                    | SlotState::Sampling { task, .. }
                    | SlotState::Done { task, .. } = state
                    {
                        let _ = task
                            .output_tx
                            .send(PipelineReply::Finished(FinishReason::Shutdown));
                        backend.clear_slot(slot.seq_id);
                    }
                    slot.n_past = 0;
                    slot.state = SlotState::Idle;
                }
                return true;
            }
            PipelineCtrlMsg::NextTokenOf(task_id) => {
                for slot in slots.iter_mut() {
                    let state = std::mem::replace(&mut slot.state, SlotState::Busy);
                    slot.state = match state {
                        SlotState::Decoding {
                            prompt_tokens,
                            logits_for_all_tokens,
                            num_written,
                            mut task,
                        } => {
                            if task.config.task_id == task_id
                                && task.config.output_mode == OutputMode::Single
                            {
                                task.step_budget = task.step_budget.saturating_add(1);
                            }
                            SlotState::Decoding {
                                prompt_tokens,
                                logits_for_all_tokens,
                                num_written,
                                task,
                            }
                        }
                        SlotState::Sampling { logits, mut task } => {
                            if task.config.task_id == task_id
                                && task.config.output_mode == OutputMode::Single
                            {
                                task.step_budget = task.step_budget.saturating_add(1);
                            }
                            SlotState::Sampling { logits, task }
                        }
                        other => other,
                    }
                }
            }
            PipelineCtrlMsg::CancelTask(task_id) => {
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
                            task,
                        } => {
                            if task.config.task_id == task_id {
                                SlotState::Done {
                                    task,
                                    reason: FinishReason::Cancelled,
                                }
                            } else {
                                SlotState::Decoding {
                                    prompt_tokens,
                                    logits_for_all_tokens,
                                    num_written,
                                    task,
                                }
                            }
                        }
                        SlotState::Sampling { logits, task } => {
                            if task.config.task_id == task_id {
                                SlotState::Done {
                                    task,
                                    reason: FinishReason::Cancelled,
                                }
                            } else {
                                SlotState::Sampling { logits, task }
                            }
                        }
                        SlotState::Done { task, reason } => {
                            if task.config.task_id == task_id {
                                SlotState::Done {
                                    task,
                                    reason: FinishReason::Cancelled,
                                }
                            } else {
                                SlotState::Done { task, reason }
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

fn remove_cancelled_inputs(queue: &mut VecDeque<PipelineTask>, task_id: &str) {
    let mut kept = VecDeque::with_capacity(queue.len());
    while let Some(input) = queue.pop_front() {
        if input.config.task_id == task_id {
            let _ = input
                .output_tx
                .send(PipelineReply::Finished(FinishReason::Cancelled));
        } else {
            kept.push_back(input);
        }
    }
    *queue = kept;
}

enum SchedResult {
    Scheduled,
    Deferred(PipelineTask),
    Rejected,
}

fn sched<B: PipelineBackend>(
    mut input: PipelineTask,
    _max_parallel: usize,
    slot_match_threshold: f32,
    backend: &mut B,
    slots: &mut [Slot],
) -> SchedResult {
    if input.prompt_tokens.is_empty() {
        let _ = input
            .output_tx
            .send(PipelineReply::Finished(FinishReason::Error(
                "prompt_tokens must not be empty".to_string(),
            )));
        return SchedResult::Rejected;
    }

    // Slot selection follows llama-use1 semantics:
    // seq_hint first, then prefix reuse threshold, then LRU fallback.
    let (slot_idx, allow_cache_reuse) = match pick_idle_slot(
        backend,
        slots,
        input.seq_hint,
        &input.prompt_tokens,
        input.use_cache,
        slot_match_threshold,
    ) {
        Some(v) => v,
        None => return SchedResult::Deferred(input),
    };

    let seq_id = slots[slot_idx].seq_id;
    let n_common = if allow_cache_reuse {
        match backend.prepare_slot(seq_id, &input.prompt_tokens) {
            Ok(n) => n,
            Err(e) => {
                let _ = input
                    .output_tx
                    .send(PipelineReply::Finished(FinishReason::Error(e)));
                return SchedResult::Rejected;
            }
        }
    } else {
        backend.clear_slot(seq_id);
        backend.truncate_slot_cache(seq_id, 0);
        0
    };

    input.prompt_tokens.drain(..n_common);
    if input.prompt_tokens.is_empty() {
        let _ = input
            .output_tx
            .send(PipelineReply::Finished(FinishReason::Error(
                "prompt_tokens became empty after prefix match".to_string(),
            )));
        return SchedResult::Rejected;
    }

    let config = input.config;
    let is_single_mode = config.output_mode == OutputMode::Single;
    let task = TaskState {
        config,
        step_budget: if is_single_mode { 1 } else { 0 },
        generated_tokens: Vec::new(),
        generated_text: String::new(),
        pending_bytes: Vec::new(),
        output_tx: input.output_tx,
    };

    let slot = &mut slots[slot_idx];
    slot.n_past = n_common as i32;
    slot.mtime = now_unix_s();
    slot.state = SlotState::Decoding {
        prompt_tokens: VecDeque::from(input.prompt_tokens),
        logits_for_all_tokens: task.config.logits_for_all_tokens,
        num_written: 0,
        task,
    };

    SchedResult::Scheduled
}

fn decoding_stage<B: PipelineBackend>(
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
        if cnt == backend.n_ubatch() {
            break;
        }

        let idx = (*slot_cursor + offset) % slots_len;
        let slot = &mut slots[idx];
        let state = std::mem::replace(&mut slot.state, SlotState::Busy);

        if let SlotState::Decoding {
            mut prompt_tokens,
            logits_for_all_tokens,
            mut num_written,
            task,
        } = state
        {
            let n_tokens = prompt_tokens.len();
            let n_take = n_tokens.min(backend.n_ubatch().saturating_sub(cnt));

            let mut feed_count = 0usize;
            let mut decode_err: Option<String> = None;
            for token in prompt_tokens.drain(..n_take) {
                // Request logits for all prompt tokens only when explicitly needed.
                let need_logits = task.config.logits_for_all_tokens || n_take == n_tokens;
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
                    task,
                    reason: FinishReason::Error(e),
                };
                continue;
            }

            slot.state = if prompt_tokens.is_empty() {
                if logits_for_all_tokens {
                    SlotState::Done {
                        task,
                        reason: FinishReason::PrefillOnly,
                    }
                } else {
                    has_sampling = true;
                    SlotState::Sampling {
                        logits: cnt as i32 - 1,
                        task,
                    }
                }
            } else {
                SlotState::Decoding {
                    prompt_tokens,
                    logits_for_all_tokens,
                    num_written,
                    task,
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
                        SlotState::Decoding { task, .. } | SlotState::Sampling { task, .. } => {
                            SlotState::Done {
                                task,
                                reason: FinishReason::Error(e.clone()),
                            }
                        }
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

fn sampling_stage<B: PipelineBackend>(backend: &mut B, slots: &mut [Slot]) -> bool {
    let mut did_work = false;

    for slot in slots.iter_mut() {
        let state = std::mem::replace(&mut slot.state, SlotState::Busy);
        slot.state = match state {
            SlotState::Sampling { logits, mut task } => {
                let can_sample = match task.config.output_mode {
                    OutputMode::Continuous | OutputMode::OneShot => true,
                    OutputMode::Single => task.step_budget > 0,
                };

                if !can_sample {
                    SlotState::Sampling { logits, task }
                } else {
                    if task.config.output_mode == OutputMode::Single {
                        task.step_budget = task.step_budget.saturating_sub(1);
                    }

                    did_work = true;
                    // Sampling consumes logits generated by the previous decode stage.
                    if let Err(e) = backend.ensure_slot_sampler(
                        slot.seq_id,
                        task.config.temperature,
                        task.config.top_p,
                    ) {
                        slot.state = SlotState::Done {
                            task,
                            reason: FinishReason::Error(e),
                        };
                        continue;
                    }
                    match backend.sample(slot.seq_id, logits) {
                        Ok(token) => {
                            if task.config.output_mode != OutputMode::OneShot {
                                let _ = task.output_tx.send(PipelineReply::Token(token));
                            }
                            task.generated_tokens.push(token);
                            let piece = backend.token_to_piece(token);
                            append_utf8_piece(
                                &mut task.generated_text,
                                &mut task.pending_bytes,
                                &piece,
                            );

                            if backend.is_eog(token) {
                                SlotState::Done {
                                    task,
                                    reason: FinishReason::Eog,
                                }
                            } else if task.config.max_tokens > 0
                                && task.generated_tokens.len() >= task.config.max_tokens
                            {
                                SlotState::Done {
                                    task,
                                    reason: FinishReason::Length,
                                }
                            } else if hit_stop_words(
                                &mut task.generated_text,
                                &task.config.stop_words,
                            ) {
                                SlotState::Done {
                                    task,
                                    reason: FinishReason::StopSequence,
                                }
                            } else {
                                SlotState::Decoding {
                                    prompt_tokens: VecDeque::from([token]),
                                    logits_for_all_tokens: false,
                                    num_written: 0,
                                    task,
                                }
                            }
                        }
                        Err(e) => SlotState::Done {
                            task,
                            reason: FinishReason::Error(e),
                        },
                    }
                }
            }
            other => other,
        };
    }

    did_work || done_stage(backend, slots) > 0
}

fn done_stage<B: PipelineBackend>(backend: &mut B, slots: &mut [Slot]) -> usize {
    let mut completed = 0usize;

    for slot in slots.iter_mut() {
        let state = std::mem::replace(&mut slot.state, SlotState::Busy);
        slot.state = match state {
            SlotState::Done { task, reason } => {
                // Finalize output, release KV/cache state, and return slot to idle.
                let hard_clear = matches!(
                    reason,
                    FinishReason::Cancelled | FinishReason::Shutdown | FinishReason::Error(_)
                );
                if task.config.output_mode == OutputMode::OneShot {
                    if hard_clear {
                        let _ = task.output_tx.send(PipelineReply::Finished(reason));
                    } else {
                        let _ = task
                            .output_tx
                            .send(PipelineReply::Tokens(task.generated_tokens.clone()));
                    }
                } else {
                    let _ = task.output_tx.send(PipelineReply::Finished(reason));
                }
                if hard_clear {
                    backend.clear_slot(slot.seq_id);
                    slot.n_past = 0;
                } else {
                    // Keep KV/cache for prompt reuse across future tasks.
                    backend.reset_slot_runtime(slot.seq_id);
                }
                completed += 1;
                SlotState::Idle
            }
            other => other,
        };
    }

    completed
}

fn count_active_tasks(slots: &[Slot]) -> usize {
    slots
        .iter()
        .filter(|slot| !matches!(slot.state, SlotState::Idle))
        .count()
}

fn hit_stop_words(generated_text: &mut String, stop_words: &[String]) -> bool {
    for stop in stop_words.iter().filter(|s| !s.is_empty()) {
        if generated_text.ends_with(stop) {
            let new_len = generated_text.len().saturating_sub(stop.len());
            generated_text.truncate(new_len);
            return true;
        }
    }
    false
}

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

fn pick_idle_slot<B: PipelineBackend>(
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

    impl PipelineBackend for MockEngine {
        fn n_ubatch(&self) -> usize {
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

        fn ensure_slot_sampler(
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

    fn recv_until_finished(
        rx: &mpsc::Receiver<PipelineReply>,
        timeout: Duration,
    ) -> Vec<PipelineReply> {
        let mut out = Vec::new();
        for _ in 0..200 {
            match rx.recv_timeout(timeout) {
                Ok(msg) => {
                    let done = matches!(msg, PipelineReply::Finished(_));
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
    fn pipeline_prefill_decode_sample_until_eog() {
        let engine = MockEngine::new(
            vec![MockSlotPlan {
                outputs: vec![101, 102],
            }],
            Some(102),
            8,
        );
        let handle = PipelineEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("t1", vec![1, 2, 3], Vec::new(), OutputMode::Continuous)
            .expect("submit t1");

        let out = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(out.iter().any(|m| matches!(m, PipelineReply::Token(101))));
        assert!(out.iter().any(|m| matches!(m, PipelineReply::Token(102))));
        assert!(
            out.iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Eog)))
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
        let handle = PipelineEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("s1", vec![9], Vec::new(), OutputMode::Single)
            .expect("submit s1");

        let first = recv_until_finished(&rx, Duration::from_millis(50));
        assert!(first.iter().any(|m| matches!(m, PipelineReply::Token(1))));
        assert!(!first.iter().any(|m| matches!(m, PipelineReply::Token(2))));

        handle.next_token_of("s1").expect("next token 2");
        let second = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(second.iter().any(|m| matches!(m, PipelineReply::Token(2))));

        handle.next_token_of("s1").expect("next token 3");
        let third = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(third.iter().any(|m| matches!(m, PipelineReply::Token(3))));
        assert!(
            third
                .iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Eog)))
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
        let handle = PipelineEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("o1", vec![5], Vec::new(), OutputMode::OneShot)
            .expect("submit o1");

        let out = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(!out.iter().any(|m| matches!(m, PipelineReply::Token(_))));
        assert!(out.iter().any(|m| matches!(
            m,
            PipelineReply::Tokens(tokens) if tokens == &vec![11, 12, 13]
        )));
        assert!(
            out.iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Eog)))
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
        let handle = PipelineEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);
        let rx = handle
            .push_task("c1", vec![7], Vec::new(), OutputMode::Single)
            .expect("submit c1");

        // wait at least one output to ensure task is running
        let _ = rx.recv_timeout(Duration::from_millis(100));
        handle.cancel_task("c1").expect("cancel c1");

        let out = recv_until_finished(&rx, Duration::from_millis(100));
        assert!(
            out.iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Cancelled)))
        );
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
        let handle = PipelineEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);

        let rx1 = handle
            .push_task("q1", vec![10], Vec::new(), OutputMode::Continuous)
            .expect("submit q1");
        let rx2 = handle
            .push_task("q2", vec![20], Vec::new(), OutputMode::Continuous)
            .expect("submit q2");

        let out1 = recv_until_finished(&rx1, Duration::from_millis(100));
        assert!(
            out1.iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Eog)))
        );

        let out2 = recv_until_finished(&rx2, Duration::from_millis(100));
        assert!(
            out2.iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Eog)))
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
        let handle = PipelineEngine::start_with_backend(1, Duration::from_millis(1), 0.5, engine);

        let rx1 = handle
            .push_task("busy", vec![10], Vec::new(), OutputMode::Continuous)
            .expect("submit busy");
        let rx2 = handle
            .push_task("queued", vec![20], Vec::new(), OutputMode::Continuous)
            .expect("submit queued");

        let _ = rx1.recv_timeout(Duration::from_millis(100));
        handle.cancel_task("queued").expect("cancel queued");

        let out2 = recv_until_finished(&rx2, Duration::from_millis(100));
        assert!(
            out2.iter()
                .any(|m| matches!(m, PipelineReply::Finished(FinishReason::Cancelled)))
        );
        handle.shutdown();
    }
}
