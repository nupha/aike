use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

type Job = Box<dyn FnOnce() + Send + 'static>;

struct PendingJob {
    job: Job,
    on_timeout: Option<Job>,
    deadline: Option<Instant>,
}

#[derive(Clone)]
struct WorkerHandle {
    tx: mpsc::SyncSender<Job>,
    pending: Arc<AtomicUsize>,
}

impl WorkerHandle {
    fn spawn(inner: Arc<Inner>) -> Self {
        // Each worker has exactly one inbox slot.
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let pending = Arc::new(AtomicUsize::new(0));

        let pending_for_thread = pending.clone();
        std::thread::spawn(move || {
            loop {
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(job) => {
                        run_job_and_drain_pending(job, &pending_for_thread, &inner);
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { tx, pending }
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

pub(crate) struct WorkerPool {
    inner: Arc<Inner>,
}

impl WorkerPool {
    pub(crate) fn singleton() -> &'static Self {
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

    pub(crate) fn add_job<F, T>(&self, job: F, on_timeout: Option<T>) -> Result<(), &'static str>
    where
        F: FnOnce() + Send + 'static,
        T: FnOnce() + Send + 'static,
    {
        self.add_job_internal(Box::new(job), on_timeout.map(|t| Box::new(t) as Job))
    }

    fn add_job_internal(&self, job: Job, on_timeout: Option<Job>) -> Result<(), &'static str> {
        let mut st = self.inner.state.lock().unwrap();

        match try_dispatch_to_idle_worker(&mut st, job) {
            Ok(()) => Ok(()),
            Err(job) => {
                if st.workers.len() < st.max_workers {
                    // spawn one more worker
                    st.workers.push(WorkerHandle::spawn(self.inner.clone()));

                    let r = try_dispatch_to_idle_worker(&mut st, job);
                    debug_assert!(r.is_ok(), "ensure there is an idle worker, can't fail");
                    return Ok(());
                }

                let deadline = st
                    .acquire_timeout_ms
                    .map(|ms| Instant::now() + Duration::from_millis(ms));

                if st.pending_jobs.len() >= st.wait_queue_capacity {
                    Err("too many pending jobs")
                } else {
                    st.pending_jobs.push_back(PendingJob {
                        job,
                        on_timeout,
                        deadline,
                    });
                    Ok(())
                }
            }
        }
    }
}

fn pending_jobs_manager(inner: Arc<Inner>) {
    loop {
        thread::sleep(Duration::from_millis(50));

        let callbacks = collect_expired_timeout_callbacks(&inner);
        for cb in callbacks {
            cb();
        }
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
    let now = Instant::now();
    let mut ret: Option<Job> = None;

    let mut st = inner.state.lock().unwrap();
    while let Some(mut pj) = st.pending_jobs.pop_front() {
        let expired = pj.deadline.is_some_and(|d| d <= now);
        if expired {
            if let Some(cb) = pj.on_timeout.take() {
                timeout_callbacks.push(cb);
            }
            continue;
        }

        ret = Some(pj.job);
        break;
    }
    drop(st);

    for cb in timeout_callbacks {
        cb();
    }

    ret
}

fn run_job_and_drain_pending(job: Job, pending: &Arc<AtomicUsize>, inner: &Arc<Inner>) {
    // The current worker keeps consuming global pending jobs to reduce wakeups/contention.
    job();
    pending.fetch_sub(1, Ordering::Relaxed);
    while let Some(next) = try_take_pending_job(inner) {
        pending.fetch_add(1, Ordering::Relaxed);
        next();
        pending.fetch_sub(1, Ordering::Relaxed);
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
