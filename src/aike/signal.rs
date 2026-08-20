// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{LazyLock, Mutex},
    thread::ThreadId,
};

use crossbeam::channel;
use zuqe::{
    CallContext, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

thread_local! {
    // Per-thread signal handler registry.
    // Signal number -> callback (non-overlapping: last `onSignal` wins).
    static HANDLER_MAP: RefCell<HashMap<u32, JSValue>> = RefCell::new(HashMap::new());
}

/// All per-thread senders, keyed by ThreadId.
/// The C signal handler broadcasts to every registered sender.
static SIGNAL_SENDERS: LazyLock<Mutex<HashMap<ThreadId, channel::Sender<u32>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Refcount per signal number: how many threads have registered a handler.
/// When the count goes 0→1 the process-wide C handler is installed;
/// when it goes 1→0 the handler is uninstalled (restored to SIG_DFL).
static SIGNAL_REFCOUNT: LazyLock<Mutex<HashMap<u32, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Signals with aike's default cleanup handler (kill `abortOnParentExit`
/// children, then exit with 128+signal) installed at startup. `uninstall_c_handler`
/// restores this instead of SIG_DFL so removing a user handler returns to
/// aike's default cleanup behavior.
static DEFAULT_HANDLER_SIGS: LazyLock<Mutex<Vec<u32>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Kill `abortOnParentExit` children, then exit with the conventional
/// 128+signal code. Async-signal-safe (libc::kill + _exit only).
extern "C" fn default_c_signal_handler(sig_num: i32) {
    super::spawn::kill_all_children();
    unsafe { libc::_exit(128 + sig_num) }
}

/// Install the default (kill-children-then-exit) handler for `sig_num`.
fn install_default_handler(sig_num: u32) {
    DEFAULT_HANDLER_SIGS.lock().unwrap().push(sig_num);
    unsafe {
        libc::signal(sig_num as i32, default_c_signal_handler as *const () as usize);
    }
}

/// Called once at startup: install the default cleanup handler for the signals
/// a parent may want to propagate to its children. User handlers registered via
/// `Aike.onSignal` take precedence (they overwrite this via `install_c_handler`).
pub(crate) fn install_default_signal_handlers() {
    install_default_handler(libc::SIGTERM as u32);
    install_default_handler(libc::SIGINT as u32);
    #[cfg(not(windows))]
    {
        install_default_handler(libc::SIGHUP as u32);
        install_default_handler(libc::SIGQUIT as u32);
    }
}

/// Signal name -> number mapping
pub(crate) fn signal_number(name: &str) -> Option<u32> {
    Some(match name {
        // Cross-platform (available on Windows CRT)
        "SIGINT" => libc::SIGINT,
        "SIGILL" => libc::SIGILL,
        "SIGABRT" => libc::SIGABRT,
        "SIGFPE" => libc::SIGFPE,
        "SIGSEGV" => libc::SIGSEGV,
        "SIGTERM" => libc::SIGTERM,
        // Unix-only
        #[cfg(not(windows))]
        "SIGHUP" => libc::SIGHUP,
        #[cfg(not(windows))]
        "SIGQUIT" => libc::SIGQUIT,
        #[cfg(not(windows))]
        "SIGKILL" => libc::SIGKILL,
        #[cfg(not(windows))]
        "SIGPIPE" => libc::SIGPIPE,
        #[cfg(not(windows))]
        "SIGALRM" => libc::SIGALRM,
        #[cfg(not(windows))]
        "SIGBUS" => libc::SIGBUS,
        #[cfg(not(windows))]
        "SIGTRAP" => libc::SIGTRAP,
        #[cfg(not(windows))]
        "SIGURG" => libc::SIGURG,
        #[cfg(not(windows))]
        "SIGSYS" => libc::SIGSYS,
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
        #[cfg(not(windows))]
        "SIGXCPU" => libc::SIGXCPU,
        #[cfg(not(windows))]
        "SIGXFSZ" => libc::SIGXFSZ,
        #[cfg(not(windows))]
        "SIGVTALRM" => libc::SIGVTALRM,
        #[cfg(not(windows))]
        "SIGPROF" => libc::SIGPROF,
        #[cfg(not(windows))]
        "SIGWINCH" => libc::SIGWINCH,
        #[cfg(not(windows))]
        "SIGIO" => libc::SIGIO,
        _ => return None,
    } as u32)
}

/// Reverse of signal_number: convert a signal number back to its name.
pub(crate) fn signal_name(sig: u32) -> Option<&'static str> {
    Some(match sig as i32 {
        // Cross-platform (available on Windows CRT)
        n if n == libc::SIGINT => "SIGINT",
        n if n == libc::SIGILL => "SIGILL",
        n if n == libc::SIGABRT => "SIGABRT",
        n if n == libc::SIGFPE => "SIGFPE",
        n if n == libc::SIGSEGV => "SIGSEGV",
        n if n == libc::SIGTERM => "SIGTERM",
        // Unix-only
        #[cfg(not(windows))]
        n if n == libc::SIGHUP => "SIGHUP",
        #[cfg(not(windows))]
        n if n == libc::SIGQUIT => "SIGQUIT",
        #[cfg(not(windows))]
        n if n == libc::SIGKILL => "SIGKILL",
        #[cfg(not(windows))]
        n if n == libc::SIGPIPE => "SIGPIPE",
        #[cfg(not(windows))]
        n if n == libc::SIGALRM => "SIGALRM",
        #[cfg(not(windows))]
        n if n == libc::SIGBUS => "SIGBUS",
        #[cfg(not(windows))]
        n if n == libc::SIGTRAP => "SIGTRAP",
        #[cfg(not(windows))]
        n if n == libc::SIGURG => "SIGURG",
        #[cfg(not(windows))]
        n if n == libc::SIGSYS => "SIGSYS",
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
        #[cfg(not(windows))]
        n if n == libc::SIGXCPU => "SIGXCPU",
        #[cfg(not(windows))]
        n if n == libc::SIGXFSZ => "SIGXFSZ",
        #[cfg(not(windows))]
        n if n == libc::SIGVTALRM => "SIGVTALRM",
        #[cfg(not(windows))]
        n if n == libc::SIGPROF => "SIGPROF",
        #[cfg(not(windows))]
        n if n == libc::SIGWINCH => "SIGWINCH",
        #[cfg(not(windows))]
        n if n == libc::SIGIO => "SIGIO",
        _ => return None,
    })
}

/// Process-wide C signal handler — broadcasts to all registered senders
extern "C" fn c_signal_handler(sig_num: i32) {
    // signal-safe trace (only async-signal-safe syscalls)
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
                // Restore aike's default cleanup handler if one is installed
                // for this signal, else the OS default.
                let handler = if DEFAULT_HANDLER_SIGS.lock().unwrap().contains(&sig_num) {
                    default_c_signal_handler as *const () as usize
                } else {
                    libc::SIG_DFL
                };
                libc::signal(sig_num as i32, handler);
            }
        }
    }
}

/// GC trace: visit all stored JSValue callbacks so the GC can keep them alive
pub fn gc_trace_signals(gcx: &mut GcTraceCtx) {
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
    crate::helpers::start_channel_poller(
        ctx,
        "signal",
        rx,
        |sig_num, cx| {
            let cb = HANDLER_MAP.with(|map| map.borrow().get(&sig_num).copied());
            if let Some(cb) = cb
                && cb.call(cx, JSValue::UNDEFINED, &[]).is_err()
            {
                let _e = cx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] signal.rs:{}: {:?}", line!(), _e);
            }
            true // always continue polling
        },
        || !HANDLER_MAP.with(|map| map.borrow().is_empty()),
        || {}, // no persistent state to clear for signals
    );
}

/// Build a platform-aware `Aike.signals` object.
/// Only signals available on the current platform are included.
/// The value is the numeric signal number (e.g. `Aike.signals.SIGTERM === 15`).
pub(super) fn js_signals(ctx: &mut ScopedContext) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;

    // Cross-platform signals (available on Windows CRT)
    obj.install_fields(
        ctx,
        &[
            FieldDef::INT32("SIGINT", libc::SIGINT, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGILL", libc::SIGILL, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGABRT", libc::SIGABRT, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGFPE", libc::SIGFPE, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGSEGV", libc::SIGSEGV, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGTERM", libc::SIGTERM, JS_PROP_ENUMERABLE),
        ],
    )?;

    // Unix-only signals
    #[cfg(not(windows))]
    obj.install_fields(
        ctx,
        &[
            FieldDef::INT32("SIGHUP", libc::SIGHUP, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGQUIT", libc::SIGQUIT, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGKILL", libc::SIGKILL, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGPIPE", libc::SIGPIPE, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGALRM", libc::SIGALRM, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGBUS", libc::SIGBUS, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGTRAP", libc::SIGTRAP, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGURG", libc::SIGURG, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGSYS", libc::SIGSYS, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGUSR1", libc::SIGUSR1, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGUSR2", libc::SIGUSR2, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGCHLD", libc::SIGCHLD, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGCONT", libc::SIGCONT, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGSTOP", libc::SIGSTOP, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGTSTP", libc::SIGTSTP, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGTTIN", libc::SIGTTIN, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGTTOU", libc::SIGTTOU, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGXCPU", libc::SIGXCPU, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGXFSZ", libc::SIGXFSZ, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGVTALRM", libc::SIGVTALRM, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGPROF", libc::SIGPROF, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGWINCH", libc::SIGWINCH, JS_PROP_ENUMERABLE),
            FieldDef::INT32("SIGIO", libc::SIGIO, JS_PROP_ENUMERABLE),
        ],
    )?;

    Ok(obj.into())
}

/// Register `handler` (a JS-callable) for the named signal on the current
/// thread. Internal native API used by both `Aike.onSignal` and other Rust
/// code that wants to hook a signal without going through the JS binding.
///
/// On first registration for a signal it installs the process-wide C handler
/// and lazily starts the per-thread polling microtask.
/// Signals the OS forbids catching or ignoring (POSIX-mandated). Installing a
/// handler for them is a silent no-op, so we reject with a hint instead.
fn is_untrappable(sig: u32) -> bool {
    #[cfg(not(windows))]
    {
        sig == libc::SIGKILL as u32 || sig == libc::SIGSTOP as u32
    }
    #[cfg(windows)]
    {
        let _ = sig;
        false
    }
}

pub(crate) fn register_signal_handler(
    ctx: &mut ScopedContext,
    sig_name: &str,
    handler: JSValue,
) -> JSResult {
    let Some(sig_num) = signal_number(sig_name) else {
        return Err(ctx.throw_type_error(&format!("unknown signal: {sig_name}")));
    };

    // SIGKILL / SIGSTOP cannot be trapped by the OS; a handler would never
    // fire, giving a false sense of safety. Ignore the request with a hint.
    if is_untrappable(sig_num) {
        eprintln!(
            "[aike] warning: cannot trap {sig_name}; signal handler ignored \
             (the OS does not allow catching SIGKILL or SIGSTOP)"
        );
        return Ok(JSValue::UNDEFINED);
    }

    handler.ensure_callable(ctx)?;

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
        SIGNAL_SENDERS
            .lock()
            .unwrap()
            .insert(std::thread::current().id(), tx);
        start_signal_microtask(ctx, rx);
    }

    if is_new {
        install_c_handler(sig_num);
    }

    Ok(JSValue::UNDEFINED)
}

/// Unregister the handler for the named signal on the current thread.
/// Returns true if a handler was actually removed. Used by both
/// `Aike.onSignal(null)` and internal native code.
pub(crate) fn unregister_signal_handler(sig_name: &str) -> bool {
    let Some(sig_num) = signal_number(sig_name) else {
        return false;
    };

    // Symmetric with register: never installed a handler for untrappable
    // signals, so there is nothing to remove.
    if is_untrappable(sig_num) {
        return false;
    }

    let tid = std::thread::current().id();

    // If this thread has no more handlers, remove its sender
    // from the global broadcast list.  The microtask will detect
    // Disconnected on next poll and return Done.
    let (is_empty, removed) = HANDLER_MAP.with(|lut| {
        let mut b = lut.borrow_mut();
        let removed = b.remove(&sig_num).is_some();
        let is_empty = b.is_empty();
        (is_empty, removed)
    });

    if is_empty {
        SIGNAL_SENDERS.lock().unwrap().remove(&tid);
    }
    if removed {
        uninstall_c_handler(sig_num);
    }
    removed
}

pub(super) fn js_on_signal(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();

    // Resolve signal name -> number
    let name_str = cc.arg(0).to_string(ctx)?;
    let sig_name = name_str.content().encode_utf8(false);

    let handler = cc.arg(1);
    if handler.is_null() {
        unregister_signal_handler(sig_name.as_ref());
    } else {
        register_signal_handler(ctx, sig_name.as_ref(), handler)?;
    }

    Ok(this)
}
