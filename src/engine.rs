use std::{
    cell::UnsafeCell,
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam::channel::{Receiver, SendError, Sender};
use llama_cpp_ffi::{LlamaContext, LlamaModel, SamplerChain, SamplerKind};

pub type Token = i32;

#[derive(Debug, Clone)]
pub struct TaskConfig {
    pub task_id: String,
    pub stop_words: Vec<String>,
    pub max_tokens: usize,
    pub temperature: f32,
    pub top_p: f32,
    pub logits_for_all_tokens: bool,
    pub output_mode: OutputMode,
}

pub struct EngineTask {
    pub config: TaskConfig,
    pub prompt_tokens: Vec<Token>,
    pub seq_hint: Option<usize>,
    pub use_cache: bool,
    pub output_tx: Sender<EngineReply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Continuous,
    OneShot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCtrlMsg {
    Shutdown,
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
    Text(String),
    Stop(StopReason, usize), // (reason, n_completion_tokens)
    Cancelled,
    Error(String),
    Shutdown,
}

#[derive(Debug)]
struct TaskState {
    config: TaskConfig,
    n_completion_tokens: usize,
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

    fn append_token(&mut self, engine: &Engine, token: Token) {
        self.n_completion_tokens += 1;
        self.append_utf8_piece(&engine.token_to_piece(token));
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

/// The parallel inference engine.
///
/// Contains both shared channel interfaces and LlamaContext worker state.
/// Worker-exclusive fields use UnsafeCell because Engine is shared via
/// `Arc<Engine>` across threads, while only the worker thread mutates
/// the LlamaContext and slot state.
pub struct Engine {
    input_tx: Sender<EngineTask>,
    ctrl_tx: Sender<EngineCtrlMsg>,
    num_tasks: AtomicUsize,
    model: LlamaModel,
    model_name: String,
    default_max_tokens: usize,
    ubatch_size: usize,

    /// Whether the HTTP server is currently running.
    is_serving: AtomicBool,
    /// Shutdown sender for the active HTTP server (if any).
    shutdown_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,

    //
    // Worker-exclusive (mutated only by the worker thread)
    //
    llm_ctx: UnsafeCell<LlamaContext>,
    slot_cached_tokens: UnsafeCell<Vec<Vec<Token>>>,
    slot_samplers: UnsafeCell<Vec<Option<SamplerChain>>>,
    pending_decode_tokens: UnsafeCell<usize>,
    worker_join_handle: UnsafeCell<Option<JoinHandle<()>>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(EngineCtrlMsg::Shutdown);

        // SAFETY: drop() has &mut self, no other references exist.
        let handle = unsafe { (*self.worker_join_handle.get()).take() };
        if let Some(jh) = handle {
            let _ = jh.join();
        }
    }
}

// Safety: UnsafeCell fields are only accessed by the worker thread.
// Other threads only touch the shared/immutable/atomic fields.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    pub fn module_name(&self) -> &str {
        &self.model_name
    }

    pub fn model(&self) -> &LlamaModel {
        &self.model
    }

    /// Private: access the UnsafeCell<LlamaContext> in worker thread only
    fn llm_ctx(&self) -> &LlamaContext {
        unsafe { &*self.llm_ctx.get() }
    }

    /// Private: access the UnsafeCell<LlamaContext> in worker thread only
    #[allow(clippy::mut_from_ref)]
    fn llm_ctx_mut(&self) -> &mut LlamaContext {
        unsafe { &mut *self.llm_ctx.get() }
    }

    fn cached_tokens(&self) -> &Vec<Vec<Token>> {
        unsafe { &*self.slot_cached_tokens.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn cached_tokens_mut(&self) -> &mut Vec<Vec<Token>> {
        unsafe { &mut *self.slot_cached_tokens.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn samplers_mut(&self) -> &mut Vec<Option<SamplerChain>> {
        unsafe { &mut *self.slot_samplers.get() }
    }

    fn pending(&self) -> usize {
        unsafe { *self.pending_decode_tokens.get() }
    }

    fn set_pending(&self, v: usize) {
        unsafe {
            *self.pending_decode_tokens.get() = v;
        }
    }

    fn common_prefix_len(&self, seq_id: usize, prompt_tokens: &[Token]) -> usize {
        let Some(cached) = self.cached_tokens().get(seq_id) else {
            return 0;
        };
        let max_common = cached.len().min(prompt_tokens.len());
        let mut n = 0usize;
        while n < max_common && cached[n] == prompt_tokens[n] {
            n += 1;
        }
        n
    }

    /// Align KV/cache state for prefix reuse. Returns reusable prefix length.
    fn prepare_slot(&self, seq_id: usize, prompt_tokens: &[Token]) -> Result<usize, String> {
        let cached = self.cached_tokens_mut();
        if seq_id >= cached.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }
        let slot_cache = &mut cached[seq_id];
        let max_common = slot_cache.len().min(prompt_tokens.len());
        let mut n_common = 0usize;
        while n_common < max_common && slot_cache[n_common] == prompt_tokens[n_common] {
            n_common += 1;
        }
        if n_common == prompt_tokens.len() && n_common > 0 {
            n_common -= 1;
        }

        let llm = self.llm_ctx_mut();
        let removed = llm.memory_seq_rm(seq_id as i32, n_common as i32, -1);
        if !removed {
            llm.memory_seq_rm(seq_id as i32, 0, -1);
            n_common = 0;
        }
        slot_cache.truncate(n_common);
        Ok(n_common)
    }

    fn truncate_slot_cache(&self, seq_id: usize, n_keep: usize) {
        if let Some(c) = self.cached_tokens_mut().get_mut(seq_id) {
            c.truncate(n_keep);
        }
    }

    fn begin_decode_batch(&self) {
        self.set_pending(0);
    }

    fn push_decode_token(
        &self,
        seq_id: usize,
        token: Token,
        pos: i32,
        need_logits: bool,
    ) -> Result<(), String> {
        let cached = self.cached_tokens_mut();
        if seq_id >= cached.len() {
            return Err(format!("invalid seq id {}", seq_id));
        } else if self.pending() >= self.ubatch_size {
            return Err("decode batch is full".to_string());
        }

        let idx = self.pending();
        let batch = self.llm_ctx_mut().batch_mut();

        unsafe {
            *batch.token.add(idx) = token;
            *batch.pos.add(idx) = pos;
            *(*batch.seq_id.add(idx)).add(0) = seq_id as i32;
            *batch.n_seq_id.add(idx) = 1;
            *batch.logits.add(idx) = if need_logits { 1 } else { 0 };
        }

        self.set_pending(self.pending() + 1);
        cached[seq_id].push(token);
        Ok(())
    }

    fn decode_batch(&self) -> Result<(), String> {
        if self.pending() > 0 {
            let lcx = self.llm_ctx_mut();
            lcx.batch_mut().n_tokens = self.pending() as i32;
            lcx.decode()
        } else {
            Ok(())
        }
    }

    fn sample(&self, seq_id: usize, logits_idx: i32) -> Result<Token, String> {
        let samplers = self.samplers_mut();
        if seq_id >= samplers.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }

        if let Some(sampler) = samplers[seq_id].as_mut() {
            Ok(sampler.sample(self.llm_ctx(), logits_idx))
        } else {
            Err(format!("slot sampler is not ready: {}", seq_id))
        }
    }

    fn init_slot_sampler(&self, seq_id: usize, temperature: f32, top_p: f32) -> Result<(), String> {
        let samplers = self.samplers_mut();
        if seq_id >= samplers.len() {
            return Err(format!("invalid seq id {}", seq_id));
        }
        if samplers[seq_id].is_none() {
            let mut sc = SamplerChain::new(self.llm_ctx().model().clone(), true);
            sc.add_sampler(&SamplerKind::Penalties {
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

            samplers[seq_id] = Some(sc);
        }
        Ok(())
    }

    fn token_to_piece(&self, token: Token) -> Vec<u8> {
        self.llm_ctx().model().token_to_piece(token)
    }

    fn is_eog(&self, token: Token) -> bool {
        self.llm_ctx().model().is_eog(token)
    }

    fn reset_slot_runtime(&self, seq_id: usize) {
        if seq_id < self.samplers_mut().len() {
            self.samplers_mut()[seq_id] = None;
        }
    }

    fn clear_slot(&self, seq_id: usize) {
        let cached = self.cached_tokens_mut();
        if seq_id < cached.len() {
            self.llm_ctx_mut().memory_seq_rm(seq_id as i32, 0, -1);
            cached[seq_id].clear();
            self.samplers_mut()[seq_id] = None;
        }
    }

    pub fn start(
        mut llm_ctx: LlamaContext,
        idle_sleep: Duration,
        slot_match_threshold: f32,
        default_max_tokens: usize,
    ) -> Arc<Self> {
        let model = llm_ctx.model().clone();
        let model_name = model
            .meta_val_str(c"general.name")
            .unwrap_or_else(|| model.model_desc());

        let num_parallel = llm_ctx.n_seq_max().max(1) as usize;
        let ubatch_size = (llm_ctx.n_ubatch() as usize).max(1);
        llm_ctx.init_batch(ubatch_size as i32, 0, llm_ctx.n_seq_max());

        let (input_tx, input_rx) = crossbeam::channel::bounded::<EngineTask>(16);
        let (ctrl_tx, ctrl_rx) = crossbeam::channel::bounded::<EngineCtrlMsg>(1);

        let engine = Arc::new(Engine {
            input_tx,
            ctrl_tx,
            num_tasks: AtomicUsize::new(0),
            model,
            model_name,
            default_max_tokens,
            ubatch_size,
            is_serving: AtomicBool::new(false),
            shutdown_tx: Mutex::new(None),
            llm_ctx: UnsafeCell::new(llm_ctx),
            slot_cached_tokens: UnsafeCell::new(vec![Vec::new(); num_parallel]),
            slot_samplers: UnsafeCell::new((0..num_parallel).map(|_| None).collect()),
            pending_decode_tokens: UnsafeCell::new(0),
            worker_join_handle: UnsafeCell::new(None),
        });

        let worker_join = thread::spawn({
            // Pass a raw pointer instead of Arc<Engine> to avoid a reference cycle:
            // - Worker holding Arc would prevent Engine::drop (last Arc never reaches 0).
            // - Even if we manually send shutdown before dropping, the worker's final
            //   Arc drop would trigger Engine::drop on the worker thread itself,
            //   causing a self-join deadlock. Raw pointer avoids both issues.
            let engine_ptr = EnginePtr(Arc::as_ptr(&engine));

            move || {
                worker_loop(
                    engine_ptr,
                    num_parallel,
                    idle_sleep,
                    slot_match_threshold,
                    input_rx,
                    ctrl_rx,
                );
            }
        });

        // SAFETY: Arc was just created, no other references exist yet.
        unsafe {
            *engine.worker_join_handle.get() = Some(worker_join);
        }

        engine
    }

    pub fn active_tasks(&self) -> usize {
        self.num_tasks.load(Ordering::Relaxed)
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn default_max_tokens(&self) -> usize {
        self.default_max_tokens
    }

    pub fn is_serving(&self) -> bool {
        self.is_serving.load(Ordering::Relaxed)
    }

    /// Mark engine as serving and store the shutdown sender.
    /// Panics in debug builds if already serving.
    pub fn set_serving(&self, tx: tokio::sync::oneshot::Sender<()>) {
        debug_assert!(!self.is_serving(), "engine is already serving");
        self.is_serving.store(true, Ordering::Relaxed);
        *self.shutdown_tx.lock().unwrap() = Some(tx);
    }

    /// Mark engine as not serving and clear the shutdown sender.
    /// Panics in debug builds if not currently serving.
    pub fn unset_serving(&self) {
        debug_assert!(self.is_serving(), "engine is not serving");
        self.is_serving.store(false, Ordering::Relaxed);
        *self.shutdown_tx.lock().unwrap() = None;
    }

    pub fn request_stop(&self) {
        if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(());
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

    pub fn cancel_task(&self, task_id: impl Into<String>) -> Result<(), SendError<EngineCtrlMsg>> {
        self.ctrl_tx.send(EngineCtrlMsg::CancelTask(task_id.into()))
    }

    fn decoding_stage(&self, slots: &mut [Slot], slot_cursor: &mut usize) -> bool {
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

    fn sampling_stage(&self, slots: &mut [Slot]) -> bool {
        let mut did_work = false;

        for slot in slots.iter_mut() {
            let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);
            if let SlotState::Sampling {
                logits,
                mut task_state,
            } = state0
            {
                if task_state.output_tx.is_full() {
                    slot.state = SlotState::Sampling { logits, task_state };
                } else if let Err(e) = self.init_slot_sampler(
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

                    match self.sample(slot.seq_id, logits) {
                        Ok(token) => {
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
                                task_state.append_token(self, token);

                                if let Some(w) = hit_stop_words(
                                    &mut task_state.generated_text,
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

    fn done_stage(&self, slots: &mut [Slot]) -> usize {
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

    fn tick(&self, slots: &mut [Slot], slot_cursor: &mut usize) -> bool {
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

    fn handle_ctrl_messages(
        &self,
        ctrl_rx: &Receiver<EngineCtrlMsg>,
        slots: &mut [Slot],
        ready: &mut VecDeque<EngineTask>,
        deferred: &mut VecDeque<EngineTask>,
        cancelled_ids: &mut HashSet<String>,
    ) -> bool {
        while let Ok(ctrl) = ctrl_rx.try_recv() {
            match ctrl {
                EngineCtrlMsg::Shutdown => {
                    for slot in slots.iter_mut() {
                        let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

                        if let SlotState::Decoding { task_state, .. }
                        | SlotState::Sampling { task_state, .. }
                        | SlotState::Done { task_state, .. } = state0
                        {
                            let _ = task_state.output_tx.send(EngineReply::Shutdown);
                            self.clear_slot(slot.seq_id);
                        }
                        slot.n_past = 0;
                        slot.state = SlotState::Idle;
                    }
                    return true;
                }
                EngineCtrlMsg::CancelTask(task_id) => {
                    remove_cancelled_inputs(ready, &task_id);
                    remove_cancelled_inputs(deferred, &task_id);
                    cancelled_ids.insert(task_id.clone());

                    for slot in slots.iter_mut() {
                        let state0 = std::mem::replace(&mut slot.state, SlotState::Busy);

                        slot.state = match state0 {
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

    fn pick_idle_slot(
        &self,
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
            let can_reuse = use_cache && self.common_prefix_len(slot.seq_id, prompt_tokens) > 0;
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
                let common = self.common_prefix_len(slots[*idx].seq_id, prompt_tokens);
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

    fn sched_task(
        &self,
        task: EngineTask,
        slots: &mut [Slot],
        prefix_match_threshold: f32,
    ) -> SchedResult {
        if task.prompt_tokens.is_empty() {
            let _ = task.output_tx.send(EngineReply::Error(
                "prompt_tokens must not be empty".to_string(),
            ));
            return SchedResult::Rejected;
        }
        let Some((slot_idx, allow_cache_reuse)) = self.pick_idle_slot(
            slots,
            task.seq_hint,
            &task.prompt_tokens,
            task.use_cache,
            prefix_match_threshold,
        ) else {
            return SchedResult::Deferred(task);
        };

        let seq_id = slots[slot_idx].seq_id;
        let n_common = if allow_cache_reuse {
            match self.prepare_slot(seq_id, &task.prompt_tokens) {
                Ok(n) => n,
                Err(e) => {
                    let _ = task.output_tx.send(EngineReply::Error(e));
                    return SchedResult::Rejected;
                }
            }
        } else {
            self.clear_slot(seq_id);
            self.truncate_slot_cache(seq_id, 0);
            0
        };

        let mut task = task;
        task.prompt_tokens.drain(..n_common);
        if task.prompt_tokens.is_empty() {
            let _ = task.output_tx.send(EngineReply::Error(
                "prompt_tokens became empty after prefix match".to_string(),
            ));
            return SchedResult::Rejected;
        }

        let task_state = TaskState {
            config: task.config,
            n_completion_tokens: 0,
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
}

/// Wrapper to send Engine pointer across thread boundaries.
#[repr(transparent)]
struct EnginePtr(*const Engine);
unsafe impl Send for EnginePtr {}

fn worker_loop(
    engine: EnginePtr,
    max_parallel: usize,
    idle_sleep: Duration,
    slot_match_threshold: f32,
    input_rx: Receiver<EngineTask>,
    ctrl_rx: Receiver<EngineCtrlMsg>,
) {
    // Raw pointer instead of Arc<Engine> to avoid reference cycle and self-join deadlock
    // (see comment at EnginePtr creation site for details).
    // SAFETY: engine pointer is valid for the worker's lifetime (Engine::drop joins the worker).
    let engine = unsafe { &*engine.0 };

    let mut slots = (0..max_parallel).map(Slot::new).collect::<Vec<_>>();
    let mut slot_cursor = 0usize;

    let mut ready_inputs = VecDeque::<EngineTask>::new();
    let mut deferred_inputs = VecDeque::<EngineTask>::new();

    let mut cancelled_ids = HashSet::<String>::new();

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

fn hit_stop_words(generated_text: &mut String, stop_words: &[String]) -> Option<String> {
    for stop in stop_words.iter().filter(|s| !s.is_empty()) {
        if generated_text.ends_with(stop) {
            return Some(stop.to_string());
        }
    }
    None
}

pub(crate) fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
