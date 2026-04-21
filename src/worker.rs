use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, Sender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use llama_cpp_ffi::{LlamaContext, SamplerChain, SamplerKind, SamplingParams, llama_vocab};

type Job = Box<dyn FnOnce() + Send + 'static>;

enum ControlMsg {
    PurgeSampler {
        sampler_id: usize,
        ack: Sender<()>,
    },
    #[cfg(debug_assertions)]
    QuerySamplerCacheCount {
        sampler_id: usize,
        reply: Sender<usize>,
    },
}

struct PendingJob {
    job: Job,
    on_timeout: Option<Job>,
    deadline: Option<Instant>,
}

#[derive(Clone)]
struct WorkerHandle {
    tx: mpsc::SyncSender<Job>,
    ctrl_tx: Sender<ControlMsg>,
    pending: Arc<AtomicUsize>,
}

impl WorkerHandle {
    fn spawn(inner: Arc<Inner>) -> Self {
        // Each worker has exactly one inbox slot.
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<ControlMsg>();
        let pending = Arc::new(AtomicUsize::new(0));

        let pending_for_thread = pending.clone();
        std::thread::spawn(move || {
            loop {
                drain_control_messages(&ctrl_rx);

                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(job) => {
                        run_job_and_drain_pending(job, &pending_for_thread, &inner, &ctrl_rx);
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            tx,
            ctrl_tx,
            pending,
        }
    }

    fn pending_len(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
}

struct WorkerState {
    workers: Vec<WorkerHandle>,
    min_workers: usize,
    max_workers: usize,
    wait_queue_capacity: usize,
    acquire_timeout_ms: Option<u64>,
    pending_jobs: VecDeque<PendingJob>,
}

struct Inner {
    state: Mutex<WorkerState>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct SamplerCacheKey {
    sampler_id: usize,
    rev: u64,
}

thread_local! {
    static WORKER_SAMPLER_CACHE: RefCell<std::collections::HashMap<SamplerCacheKey, SamplerChain<'static>>> =
        RefCell::new(std::collections::HashMap::new());
}

static SAMPLER_REVISIONS: OnceLock<Mutex<std::collections::HashMap<usize, u64>>> = OnceLock::new();

fn sampler_revisions() -> &'static Mutex<std::collections::HashMap<usize, u64>> {
    SAMPLER_REVISIONS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

pub(crate) struct WorkerPool {
    inner: Arc<Inner>,
}

impl WorkerPool {
    pub(crate) fn global() -> &'static Self {
        static POOL: OnceLock<WorkerPool> = OnceLock::new();
        POOL.get_or_init(|| {
            let cpu = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);

            let parse_usize = |name: &str| -> Option<usize> {
                std::env::var(name)
                    .ok()
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .filter(|v| *v > 0)
            };

            let parse_u64 = |name: &str| -> Option<u64> {
                std::env::var(name)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|v| *v > 0)
            };

            let default_max_workers = (cpu / 2).clamp(2, 8);
            let min_workers = parse_usize("LLAMA_JS_MIN_WORKERS").unwrap_or(1usize);
            let mut max_workers =
                parse_usize("LLAMA_JS_MAX_WORKERS").unwrap_or(default_max_workers);
            if max_workers < min_workers {
                max_workers = min_workers;
            }

            let wait_queue_capacity = parse_usize("LLAMA_JS_WAIT_QUEUE_CAPACITY").unwrap_or(4096);
            let acquire_timeout_ms = parse_u64("LLAMA_JS_ACQUIRE_TIMEOUT_MS");

            let inner = Arc::new(Inner {
                state: Mutex::new(WorkerState {
                    workers: Vec::new(),
                    min_workers,
                    max_workers,
                    wait_queue_capacity,
                    acquire_timeout_ms,
                    pending_jobs: VecDeque::new(),
                }),
            });

            {
                let mut st = inner.state.lock().unwrap();
                let min_workers = st.min_workers;
                for _ in 0..min_workers {
                    st.workers.push(WorkerHandle::spawn(inner.clone()));
                }
            }

            {
                let inner2 = inner.clone();
                thread::spawn(move || pending_jobs_manager(inner2));
            }

            WorkerPool { inner }
        })
    }

    pub(crate) fn submit<F, T>(&self, job: F, on_timeout: Option<T>) -> Result<(), &'static str>
    where
        F: FnOnce() + Send + 'static,
        T: FnOnce() + Send + 'static,
    {
        self.submit_boxed(Box::new(job), on_timeout.map(|t| Box::new(t) as Job))
    }

    fn submit_boxed(&self, job: Job, on_timeout: Option<Job>) -> Result<(), &'static str> {
        let mut st = self.inner.state.lock().unwrap();

        let mut job = match try_dispatch_to_idle_worker(&mut st, job) {
            Ok(()) => return Ok(()),
            Err(job) => job,
        };

        if st.workers.len() < st.max_workers {
            st.workers.push(WorkerHandle::spawn(self.inner.clone()));
            job = match try_dispatch_to_idle_worker(&mut st, job) {
                Ok(()) => return Ok(()),
                Err(job) => job,
            };
        }

        let deadline = st
            .acquire_timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));

        if st.pending_jobs.len() >= st.wait_queue_capacity {
            return Err("QueueFull");
        }

        st.pending_jobs.push_back(PendingJob {
            job,
            on_timeout,
            deadline,
        });
        Ok(())
    }

    pub(crate) fn purge_sampler_sync(&self, sampler_id: usize) {
        let workers = {
            let st = self.inner.state.lock().unwrap();
            st.workers.clone()
        };
        if workers.is_empty() {
            return;
        }

        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        let mut expected = 0usize;
        for w in workers {
            let msg = ControlMsg::PurgeSampler {
                sampler_id,
                ack: ack_tx.clone(),
            };
            if w.ctrl_tx.send(msg).is_ok() {
                expected += 1;
            }
        }
        drop(ack_tx);

        for _ in 0..expected {
            if ack_rx.recv().is_err() {
                break;
            }
        }
    }
}

fn pending_jobs_manager(inner: Arc<Inner>) {
    loop {
        thread::sleep(Duration::from_millis(50));
        let callbacks = collect_expired_timeout_callbacks(&inner);
        run_timeout_callbacks(callbacks);
    }
}

fn try_dispatch_to_idle_worker(st: &mut WorkerState, mut job: Job) -> Result<(), Job> {
    let mut idx = 0usize;
    while idx < st.workers.len() {
        if st.workers[idx].pending_len() != 0 {
            idx += 1;
            continue;
        }
        match st.workers[idx].tx.try_send(job) {
            Ok(()) => {
                st.workers[idx].pending.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            Err(TrySendError::Full(j)) => {
                job = j;
                idx += 1;
            }
            Err(TrySendError::Disconnected(j)) => {
                job = j;
                st.workers.swap_remove(idx);
            }
        }
    }
    Err(job)
}

fn try_take_pending_job(inner: &Arc<Inner>) -> Option<Job> {
    let mut timeout_callbacks: Vec<Job> = Vec::new();
    let mut st = inner.state.lock().unwrap();
    let now = Instant::now();
    while let Some(mut pj) = st.pending_jobs.pop_front() {
        let expired = pj.deadline.is_some_and(|d| d <= now);
        if expired {
            if let Some(cb) = pj.on_timeout.take() {
                timeout_callbacks.push(cb);
            }
            continue;
        }
        drop(st);
        for cb in timeout_callbacks {
            cb();
        }
        return Some(pj.job);
    }
    drop(st);
    run_timeout_callbacks(timeout_callbacks);
    None
}

fn run_job_and_drain_pending(
    job: Job,
    pending: &Arc<AtomicUsize>,
    inner: &Arc<Inner>,
    ctrl_rx: &mpsc::Receiver<ControlMsg>,
) {
    // The current worker keeps consuming global pending jobs to reduce wakeups/contention.
    job();
    pending.fetch_sub(1, Ordering::Relaxed);
    drain_control_messages(ctrl_rx);
    while let Some(next) = try_take_pending_job(inner) {
        drain_control_messages(ctrl_rx);
        pending.fetch_add(1, Ordering::Relaxed);
        next();
        pending.fetch_sub(1, Ordering::Relaxed);
        drain_control_messages(ctrl_rx);
    }
}

fn collect_expired_timeout_callbacks(inner: &Arc<Inner>) -> Vec<Job> {
    let now = Instant::now();
    let mut timeout_callbacks: Vec<Job> = Vec::new();
    let mut st = inner.state.lock().unwrap();
    let mut i = 0usize;
    while i < st.pending_jobs.len() {
        let expired = st
            .pending_jobs
            .get(i)
            .and_then(|pj| pj.deadline)
            .is_some_and(|d| d <= now);
        if expired {
            let mut pj = st.pending_jobs.remove(i).unwrap();
            if let Some(cb) = pj.on_timeout.take() {
                timeout_callbacks.push(cb);
            }
            continue;
        }
        i += 1;
    }
    timeout_callbacks
}

fn run_timeout_callbacks(callbacks: Vec<Job>) {
    for cb in callbacks {
        cb();
    }
}

fn drain_control_messages(ctrl_rx: &mpsc::Receiver<ControlMsg>) {
    loop {
        match ctrl_rx.try_recv() {
            Ok(ControlMsg::PurgeSampler { sampler_id, ack }) => {
                purge_sampler_cache_local(sampler_id);
                let _ = ack.send(());
            }
            #[cfg(debug_assertions)]
            Ok(ControlMsg::QuerySamplerCacheCount { sampler_id, reply }) => {
                let _ = reply.send(count_sampler_cache_local(sampler_id));
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn purge_sampler_cache_local(sampler_id: usize) {
    WORKER_SAMPLER_CACHE.with(|cache| {
        cache.borrow_mut().retain(|k, _| k.sampler_id != sampler_id);
    });
}

#[cfg(debug_assertions)]
fn count_sampler_cache_local(sampler_id: usize) -> usize {
    WORKER_SAMPLER_CACHE.with(|cache| {
        cache
            .borrow()
            .keys()
            .filter(|k| k.sampler_id == sampler_id)
            .count()
    })
}

pub(crate) fn current_sampler_rev(sampler_id: usize) -> u64 {
    sampler_revisions()
        .lock()
        .ok()
        .and_then(|m| m.get(&sampler_id).copied())
        .unwrap_or(1)
}

pub(crate) fn next_sampler_rev(sampler_id: usize) -> u64 {
    if let Ok(mut map) = sampler_revisions().lock() {
        let e = map.entry(sampler_id).or_insert(0);
        *e = e.saturating_add(1);
        *e
    } else {
        1
    }
}

pub(crate) fn sample_with_cached_sampler(
    llm_ctx: &mut LlamaContext,
    sampler_id: usize,
    sampler_rev_at_fetch: u64,
    vocab_ptr: usize,
    ctx_train: i32,
    steps: &[SamplerKind],
    logits: i32,
    params_opt: Option<&SamplingParams>,
) -> Result<i32, String> {
    let current_rev = current_sampler_rev(sampler_id);
    if current_rev != sampler_rev_at_fetch {
        return Err("Sampler was disposed".to_string());
    }

    let key = SamplerCacheKey {
        sampler_id,
        rev: current_rev,
    };

    WORKER_SAMPLER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();

        // Keep only one live version per sampler id in each worker thread.
        cache.retain(|k, _| !(k.sampler_id == sampler_id && k.rev != current_rev));

        cache.entry(key).or_insert_with(|| {
            let sampler =
                SamplerChain::new_from_vocab(vocab_ptr as *const llama_vocab, ctx_train, false);
            for kind in steps {
                sampler.add_sampler(kind);
            }
            sampler
        });

        let sampler = cache
            .get(&key)
            .ok_or_else(|| "sampler cache miss".to_string())?;

        Ok(sampler.sample_with_params(llm_ctx, logits, params_opt))
    })
}

#[cfg(debug_assertions)]
impl WorkerPool {
    pub(crate) fn debug_sampler_cache_count(&self, sampler_id: usize) -> usize {
        let workers = {
            let st = self.inner.state.lock().unwrap();
            st.workers.clone()
        };
        if workers.is_empty() {
            return 0;
        }

        let (reply_tx, reply_rx) = mpsc::channel::<usize>();
        let mut expected = 0usize;
        for w in workers {
            let msg = ControlMsg::QuerySamplerCacheCount {
                sampler_id,
                reply: reply_tx.clone(),
            };
            if w.ctrl_tx.send(msg).is_ok() {
                expected += 1;
            }
        }
        drop(reply_tx);

        let mut total = 0usize;
        for _ in 0..expected {
            match reply_rx.recv() {
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        total
    }
}
