use std::{
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};

use crossbeam::channel::{Receiver, SendError, Sender};
use llama_cpp_ffi::{LlamaContext, LlamaModel, SamplerChain, SamplerKind};

use super::{
    cache::CacheRam,
    slot::Slot,
    task::{EngineCtrlMsg, EngineReply, EngineTask, GrammarConfig, TaskConfig, Token},
    worker::worker_loop,
};

/// The parallel inference engine.
///
/// Contains both shared channel interfaces and LlamaContext worker state.
#[derive(Debug)]
pub struct Engine {
    pub(crate) input_tx: Sender<Box<EngineTask>>,
    pub(crate) ctrl_tx: Sender<EngineCtrlMsg>,
    pub(crate) num_tasks: usize,

    pub(crate) model: LlamaModel,
    pub(crate) model_name: String,
    pub(crate) default_max_tokens: usize,
    pub(crate) ubatch_size: usize,

    //
    // Worker-exclusive (mutated only by the worker thread)
    //
    pub(crate) llm_ctx: LlamaContext,
    pub(crate) pending_decode_tokens: usize,
    pub(crate) worker_join_handle: Option<JoinHandle<()>>,

    /// Whether the HTTP server is currently running.
    pub(crate) is_serving: bool,
    /// Shutdown sender for the active HTTP server (if any).
    pub(crate) shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,

    /// Whether to parse <think> tags and expose reasoning content
    /// via reasoning_content field instead of leaving them in content.
    /// meaningful for chat completion response only.
    pub(crate) parse_reasoning: bool,

    /// Minimum chunk size for KV cache shifting (cache_reuse).
    /// When > 0, after the initial prefix match, the engine scans for
    /// additional matching chunks in the cached prompt and uses
    /// memory_seq_rm + memory_seq_add to shift them into the correct
    /// position, avoiding recomputation. 0 = disabled.
    pub(crate) n_cache_reuse: usize,

    /// RAM-based prompt cache pool for KV state persistence across slots.
    pub(crate) cache_ram: CacheRam,

    /// Set to true when the worker thread has panicked.
    /// Once dead, push_task() will return an error and the engine
    /// should be considered unusable.
    pub(crate) is_dead: bool,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.ctrl_tx.send(EngineCtrlMsg::Shutdown);

        // SAFETY: drop() has &mut self, no other references exist.
        if let Some(jh) = self.worker_join_handle.take() {
            let _ = jh.join();
        }
    }
}

// Safety: UnsafeCell fields are only accessed by the worker thread.
// Other threads only touch the shared/immutable/atomic fields.
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn model(&self) -> &LlamaModel {
        &self.model
    }

    pub(super) fn llm_ctx(&self) -> &LlamaContext {
        &self.llm_ctx
    }

    pub(super) fn llm_ctx_mut(&mut self) -> &mut LlamaContext {
        &mut self.llm_ctx
    }

    pub(crate) fn pending(&self) -> usize {
        self.pending_decode_tokens
    }

    pub(crate) fn set_pending(&mut self, v: usize) {
        self.pending_decode_tokens = v;
    }

    pub(crate) fn begin_decode_batch(&mut self) {
        self.set_pending(0);
    }

    pub(crate) fn push_decode_token(
        &mut self,
        slot: &mut Slot,
        token: Token,
        pos: i32,
        need_logits: bool,
    ) -> Result<(), String> {
        let idx = self.pending();

        if idx >= self.ubatch_size {
            return Err("decode batch is full".to_string());
        }

        unsafe {
            let batch = self.llm_ctx_mut().batch_mut();

            *batch.token.add(idx) = token;
            *batch.pos.add(idx) = pos;
            *(*batch.seq_id.add(idx)).add(0) = slot.seq_id as i32;
            *batch.n_seq_id.add(idx) = 1;
            *batch.logits.add(idx) = if need_logits { 1 } else { 0 };
        }

        self.set_pending(idx + 1);
        slot.cached_tokens.push(token);

        Ok(())
    }

    pub(crate) fn decode_batch(&mut self) -> Result<(), String> {
        let pending = self.pending();
        if pending > 0 {
            let lcx = self.llm_ctx_mut();
            lcx.batch_mut().n_tokens = pending as i32;
            lcx.decode()
        } else {
            Ok(())
        }
    }

    pub(crate) fn sample(&mut self, slot: &mut Slot, logits_idx: i32) -> Result<Token, String> {
        let ctx_ptr = {
            let c = self.llm_ctx();
            c.as_ptr()
        };

        if let Some(sampler) = slot.sampler.as_mut() {
            Ok(sampler.sample(ctx_ptr, logits_idx))
        } else {
            Err(format!("slot sampler is not ready: {}", slot.seq_id))
        }
    }

    /// accept sampled token
    pub(crate) fn sample_accept(&mut self, slot: &mut Slot, token: Token) {
        if let Some(sampler) = slot.sampler.as_mut() {
            sampler.accept(token);
        }
    }

    /// lazy initialize sampler for slot
    pub(super) fn init_sampler(
        &mut self,
        slot: &mut Slot,
        temperature: f32,
        top_p: f32,
        grammar: Option<&GrammarConfig>,
    ) -> Result<(), String> {
        if slot.sampler.is_none() {
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

            if let Some(g) = grammar {
                if let Err(e) = sc.set_grammar(&g.lark_grammar) {
                    log::warn!("failed to set grammar: {}", e);
                } else {
                    sc.set_tool_choice(g.tool_choice);
                }
            }

            slot.sampler = Some(sc);
        }

        Ok(())
    }

    pub fn start(
        mut llm_ctx: LlamaContext,
        idle_sleep: Duration,
        slot_match_threshold: f32,
        default_max_tokens: usize,
        n_cache_reuse: usize,
    ) -> Arc<Mutex<Self>> {
        let model = llm_ctx.model().clone();
        let model_name = model
            .meta_val_str(c"general.name")
            .unwrap_or_else(|| model.model_desc());

        let num_parallel = llm_ctx.n_seq_max().max(1) as usize;
        let ubatch_size = (llm_ctx.n_ubatch() as usize).max(1);
        llm_ctx.init_batch(ubatch_size as i32, 0, llm_ctx.n_seq_max());

        let (input_tx, input_rx) = crossbeam::channel::bounded::<Box<EngineTask>>(16);
        let (ctrl_tx, ctrl_rx) = crossbeam::channel::bounded::<EngineCtrlMsg>(1);

        let engine = Arc::new(Mutex::new(Engine {
            input_tx,
            ctrl_tx,
            num_tasks: 0,
            model,
            model_name,
            default_max_tokens,
            ubatch_size,
            is_serving: false,
            shutdown_tx: None,
            parse_reasoning: true,
            n_cache_reuse,
            cache_ram: CacheRam::new(0), // 0 = disabled by default
            is_dead: false,
            llm_ctx,
            pending_decode_tokens: 0,
            worker_join_handle: None,
        }));

        let worker_join = thread::spawn({
            let eng2 = engine.clone();
            move || {
                // Wrap worker_loop in catch_unwind so that a panic (e.g. from
                // llama_decode) is logged and the engine is marked as stopped
                // instead of silently killing the worker thread.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(
                        eng2.clone(),
                        num_parallel,
                        idle_sleep,
                        slot_match_threshold,
                        input_rx,
                        ctrl_rx,
                    );
                }));

                if let Err(e) = result {
                    // log panic error
                    let msg = if let Some(s) = e.downcast_ref::<&str>() {
                        s.to_string()
                    } else if let Some(s) = e.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        format!("{:?}", e)
                    };
                    log::error!("[worker] worker thread panicked: {}", msg);

                    // Notify all pending tasks that the engine has crashed.
                    if let Ok(mut eng) = eng2.lock() {
                        eng.num_tasks = 0;
                        eng.is_dead = true;

                        // Send shutdown signal to HTTP server if serving.
                        eng.notify_stop();
                    }
                }
            }
        });

        engine.lock().unwrap().worker_join_handle = Some(worker_join);
        engine
    }

    pub fn active_tasks(&self) -> usize {
        self.num_tasks
    }

    /// Returns true if the worker thread is still running.
    /// Once the worker has panicked, this returns false and the engine
    /// should be considered unusable.
    pub fn is_alive(&self) -> bool {
        !self.is_dead
    }

    pub fn default_max_tokens(&self) -> usize {
        self.default_max_tokens
    }

    pub fn is_serving(&self) -> bool {
        self.is_serving
    }

    pub fn parse_reasoning(&self) -> bool {
        self.parse_reasoning
    }

    pub fn set_parse_reasoning(&mut self, v: bool) {
        self.parse_reasoning = v;
    }

    /// Mark engine as serving and store the shutdown sender.
    /// Panics in debug builds if already serving.
    pub fn set_serving(&mut self, tx: tokio::sync::oneshot::Sender<()>) {
        debug_assert!(!self.is_serving(), "engine is already serving");
        self.is_serving = true;
        self.shutdown_tx.replace(tx);
    }

    /// Mark engine as not serving and clear the shutdown sender.
    /// Panics in debug builds if not currently serving.
    pub fn unset_serving(&mut self) {
        debug_assert!(self.is_serving(), "engine is not serving");
        self.is_serving = false;
        self.shutdown_tx.take();
    }

    pub fn notify_stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }

    pub fn push_task(
        &self,
        prompt_tokens: Vec<Token>,
        config: TaskConfig,
        output_prepend: Vec<Token>,
    ) -> Result<Receiver<EngineReply>, String> {
        if self.is_dead {
            return Err("engine worker has crashed".to_string());
        }

        let (output_tx, output_rx) = crossbeam::channel::bounded::<EngineReply>(1);

        let task = Box::new(EngineTask {
            config,
            prompt_tokens,
            output_prepend,
            output_tx,
            timestamp: None,
        });
        self.input_tx.send(task).map_err(|e| e.to_string())?;

        Ok(output_rx)
    }

    pub fn cancel_task(&self, task_id: usize) -> Result<(), SendError<EngineCtrlMsg>> {
        self.ctrl_tx.send(EngineCtrlMsg::CancelTask(task_id))
    }
}
