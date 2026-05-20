// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! signal.rs — zuq.onSignal(signal, handler) for POSIX signal handling.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{LazyLock, Mutex},
    thread::ThreadId,
};

use crossbeam::channel;
use zuqe::{
    CallContext, JSResult, JSValue, JobState, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

// ---------------------------------------------------------------------------
// Thread-local state
// ---------------------------------------------------------------------------

thread_local! {
    // Per-thread signal handler registry.
    // Signal number -> callback (non-overlapping: last `onSignal` wins).
    static HANDLER_MAP: RefCell<HashMap<u32, JSValue>> = RefCell::new(HashMap::new());
}

// ---------------------------------------------------------------------------
// Global (process-wide) state
// ---------------------------------------------------------------------------

/// All per-thread senders, keyed by ThreadId.
/// The C signal handler broadcasts to every registered sender.
static SIGNAL_SENDERS: LazyLock<Mutex<HashMap<ThreadId, channel::Sender<u32>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Refcount per signal number: how many threads have registered a handler.
/// When the count goes 0→1 the process-wide C handler is installed;
/// when it goes 1→0 the handler is uninstalled (restored to SIG_DFL).
static SIGNAL_REFCOUNT: LazyLock<Mutex<HashMap<u32, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Signal name -> number mapping
pub(crate) fn signal_number(name: &str) -> Option<u32> {
    Some(match name {
        "SIGINT" => libc::SIGINT,
        "SIGABRT" => libc::SIGABRT,
        "SIGFPE" => libc::SIGFPE,
        "SIGILL" => libc::SIGILL,
        "SIGSEGV" => libc::SIGSEGV,
        "SIGTERM" => libc::SIGTERM,
        #[cfg(not(windows))]
        "SIGQUIT" => libc::SIGQUIT,
        #[cfg(not(windows))]
        "SIGPIPE" => libc::SIGPIPE,
        #[cfg(not(windows))]
        "SIGALRM" => libc::SIGALRM,
        #[cfg(not(windows))]
        "SIGUSR1" => libc::SIGUSR1,
        #[cfg(not(windows))]
        "SIGUSR2" => libc::SIGUSR2,
        #[cfg(not(windows))]
        "SIGCHLD" => libc::SIGCHLD,
        #[cfg(not(windows))]
        "SIGCONT" => libc::SIGCONT,
        #[cfg(not(windows))]
        "SIGSTOP" => libc::SIGSTOP,
        #[cfg(not(windows))]
        "SIGTSTP" => libc::SIGTSTP,
        #[cfg(not(windows))]
        "SIGTTIN" => libc::SIGTTIN,
        #[cfg(not(windows))]
        "SIGTTOU" => libc::SIGTTOU,
        _ => return None,
    } as u32)
}

/// Reverse of signal_number: convert a signal number back to its name.
pub(crate) fn signal_name(sig: u32) -> Option<&'static str> {
    Some(match sig as i32 {
        n if n == libc::SIGINT => "SIGINT",
        n if n == libc::SIGABRT => "SIGABRT",
        n if n == libc::SIGFPE => "SIGFPE",
        n if n == libc::SIGILL => "SIGILL",
        n if n == libc::SIGSEGV => "SIGSEGV",
        n if n == libc::SIGTERM => "SIGTERM",
        #[cfg(not(windows))]
        n if n == libc::SIGQUIT => "SIGQUIT",
        #[cfg(not(windows))]
        n if n == libc::SIGPIPE => "SIGPIPE",
        #[cfg(not(windows))]
        n if n == libc::SIGALRM => "SIGALRM",
        #[cfg(not(windows))]
        n if n == libc::SIGUSR1 => "SIGUSR1",
        #[cfg(not(windows))]
        n if n == libc::SIGUSR2 => "SIGUSR2",
        #[cfg(not(windows))]
        n if n == libc::SIGCHLD => "SIGCHLD",
        #[cfg(not(windows))]
        n if n == libc::SIGCONT => "SIGCONT",
        #[cfg(not(windows))]
        n if n == libc::SIGSTOP => "SIGSTOP",
        #[cfg(not(windows))]
        n if n == libc::SIGTSTP => "SIGTSTP",
        #[cfg(not(windows))]
        n if n == libc::SIGTTIN => "SIGTTIN",
        #[cfg(not(windows))]
        n if n == libc::SIGTTOU => "SIGTTOU",
        _ => return None,
    })
}

/// Process-wide C signal handler — broadcasts to all registered senders
extern "C" fn c_signal_handler(sig_num: i32) {
    // try_lock: non-blocking, safe in signal context.
    if let Ok(guard) = SIGNAL_SENDERS.try_lock() {
        for tx in guard.values() {
            // try_send: non-blocking, never blocks the signal handler.
            let _ = tx.try_send(sig_num as u32);
        }
    }
}

fn install_c_handler(sig_num: u32) {
    let mut refcount = SIGNAL_REFCOUNT.lock().unwrap();
    let count = refcount.entry(sig_num).or_insert(0);
    *count += 1;
    if *count == 1 {
        // First registration: install the process-wide C handler.
        unsafe {
            libc::signal(sig_num as i32, c_signal_handler as *const () as usize);
        }
    }
}

fn uninstall_c_handler(sig_num: u32) {
    let mut refcount = SIGNAL_REFCOUNT.lock().unwrap();
    if let Some(count) = refcount.get_mut(&sig_num) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            refcount.remove(&sig_num);
            unsafe {
                libc::signal(sig_num as i32, libc::SIG_DFL as usize);
            }
        }
    }
}

/// GC trace: visit all stored JSValue callbacks so the GC can keep them alive
pub fn gc_trace_zuq_signals(gcx: &mut GcTraceCtx) {
    HANDLER_MAP.with(|map| {
        for cb in map.borrow().values() {
            cb.trace(gcx);
        }
    });
}

/// Start the per-thread signal polling microtask.  `rx` is moved into the
/// closure so no thread-local receiver variable is needed.  When the last
/// sender for this thread is dropped (all handlers removed), the microtask
/// sees `Disconnected` and returns `Done`.
fn start_signal_microtask(ctx: &mut ScopedContext, rx: channel::Receiver<u32>) {
    ctx.queue_job(
        move |cx| loop {
            match rx.try_recv() {
                Ok(sig_num) => {
                    let cb = HANDLER_MAP.with(|map| map.borrow().get(&sig_num).copied());
                    if let Some(cb) = cb {
                        let _ = cb.call(cx, JSValue::UNDEFINED, &[]);
                    }
                }
                Err(channel::TryRecvError::Empty) => {
                    return Ok(JobState::Again);
                }
                Err(channel::TryRecvError::Disconnected) => {
                    // Sender dropped (all handlers removed) — stop.
                    return Ok(JobState::Done);
                }
            }
        },
        (),
    );
}

pub(super) fn js_on_signal(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    // Resolve signal name -> number
    let name_str = cc.arg(0).to_string(ctx)?;
    let sig_name = name_str.content().encode_utf8(false);
    let Some(sig_num) = signal_number(sig_name.as_ref()) else {
        ctx.throw_type_error(&format!("unknown signal: {sig_name}"));
        return Err(-1);
    };

    let handler = cc.arg(1);
    let tid = std::thread::current().id();

    if handler.is_null() {
        let (is_empty, removed) = HANDLER_MAP.with(|lut| {
            let mut b = lut.borrow_mut();
            let removed = b.remove(&sig_num).is_some();
            let is_empty = b.is_empty();
            (is_empty, removed)
        });

        // If this thread has no more handlers, remove its sender
        // from the global broadcast list.  The microtask will detect
        // Disconnected on next poll and return Done.
        if is_empty {
            SIGNAL_SENDERS.lock().unwrap().remove(&tid);
        }

        if removed {
            uninstall_c_handler(sig_num);
        }
    } else {
        // --- Register handler ---
        handler.ensure_function(ctx)?;

        // Atomically: check emptiness, detect new signal, store callback.
        let (was_empty, is_new) = HANDLER_MAP.with(|map| {
            let mut b = map.borrow_mut();
            let was_empty = b.is_empty();
            let existed = b.contains_key(&sig_num);
            b.insert(sig_num, handler);
            (was_empty, !existed)
        });

        // Lazy-init the per-thread channel on first registration.
        // When HANDLER_MAP was empty, the previous microtask has already
        // exited (Disconnected), so we need a fresh channel + microtask.
        if was_empty {
            let (tx, rx) = channel::bounded::<u32>(16);
            SIGNAL_SENDERS.lock().unwrap().insert(tid, tx);
            start_signal_microtask(ctx, rx);
        }

        if is_new {
            install_c_handler(sig_num);
        }
    }

    Ok(this)
}
