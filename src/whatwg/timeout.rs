// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Timers Standard: setTimeout, setInterval, clearTimeout, clearInterval.
//!
//! Pending timers live in a single `Vec` kept sorted by `deadline` ascending, so
//! `entries[0]` is always the nearest deadline.  A single self-requeuing JS job
//! fires due timers in that order, driven entirely by the engine's job loop
//! ticking this job (`JobState::Again`); there is no separate timer thread or
//! per-timer sleep task.

use std::{
    cell::{Cell, RefCell},
    time::{Duration, Instant},
};

use zuqe::{
    CallContext, Callback, Exception, JS_PROP_C_W_E, JSResult, JSValue, JobState, Object,
    ScopedContext,
    builtins::promise::Promise,
    gc_lite::{GcTrace, GcTraceCtx},
};

// ---------------------------------------------------------------------------
// Per-timer entry stored in the registry
// ---------------------------------------------------------------------------

struct TimerEntry {
    /// Stable id returned to JS and used by clearTimeout/clearInterval. Never
    /// reused, so a stale clear can't hit a later timer that recycled the id.
    id: u32,
    /// JS function callback — must be GC-traced
    callback: Object,
    /// None for setTimeout; Some(ms) for setInterval
    interval_ms: Option<u64>,
    /// Absolute deadline captured at registration (and on every re-arm for
    /// intervals). The `entries` vec is kept sorted by this, so firing order is
    /// deterministic and `entries[0]` is always the nearest deadline.
    deadline: Instant,
}

// ---------------------------------------------------------------------------
// Thread-local global state
// ---------------------------------------------------------------------------

struct TimerState {
    next_id: u32,
    /// Pending timers, kept sorted by `deadline` ascending. `entries[0]` is
    /// always the nearest deadline, so firing order and the per-tick early
    /// return need no separate index or cached timestamp.
    entries: Vec<TimerEntry>,
}

impl TimerState {
    fn new() -> Self {
        let next_id = TIMER_SERIAL.with(|t| {
            let id = t.get();
            t.set(id + 1);
            id
        });
        Self {
            next_id,
            entries: Vec::new(),
        }
    }

    fn timer_id(&mut self) -> u32 {
        let id = TIMER_SERIAL.with(|t| {
            let id = t.get();
            t.set(id + 1);
            id
        });
        self.next_id = id;
        id
    }

    /// Insert an entry keeping `entries` sorted by `deadline` ascending.
    fn insert_sorted(&mut self, entry: TimerEntry) {
        let pos = self
            .entries
            .partition_point(|e| e.deadline <= entry.deadline);
        self.entries.insert(pos, entry);
    }
}

thread_local! {
    // Some(_) = polling microtask is active; None = no background job running.
    static TIMER_STATE: RefCell<Option<TimerState>> = const { RefCell::new(None) };

    static TIMER_SERIAL: Cell<u32> = const { Cell::new(1) };
}

// ---------------------------------------------------------------------------
// GC trace — called by the runtime GC to keep all live callbacks alive
// ---------------------------------------------------------------------------

pub(crate) fn gc_trace_timeout(tr: &mut GcTraceCtx) {
    TIMER_STATE.with_borrow(|opt| {
        if let Some(s) = opt.as_ref() {
            for entry in s.entries.iter() {
                entry.callback.trace(tr);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// The single persistent polling microtask
// ---------------------------------------------------------------------------

/// Enqueue the polling microtask.  Called only when TIMER_STATE is Some
/// (state already initialized by the caller).
///
/// The job self-requeues (`JobState::Again`) while timers are pending.  On each
/// tick it first compares `entries[0].deadline` (the nearest, since `entries`
/// is sorted) once against `Instant::now()` and bails immediately when nothing
/// is due, so the idle cost is a single comparison.  When a timer is due it
/// fires every elapsed timer in **deadline order**; execution order is
/// deterministic.  A timer that was cleared (entry removed) is simply never
/// fired.
fn start_timer_task(ctx: &mut ScopedContext) {
    #[cfg(debug_assertions)]
    TIMER_STATE.with_borrow_mut(|opt| {
        debug_assert!(opt.is_some());
    });

    ctx.queue_job(
        move |cx| {
            let now = Instant::now();

            // Fast path: if the nearest timer (`entries[0]`) isn't due yet, or
            // there are no timers at all, bail.  Costs a single comparison.
            match TIMER_STATE.with_borrow(|s| {
                s.as_ref()
                    .and_then(|s| s.entries.first())
                    .map(|e| e.deadline)
            }) {
                None => {
                    TIMER_STATE.with_borrow_mut(|s| {
                        s.take();
                    });
                    return Ok(JobState::Done);
                }
                Some(t) if now < t => {
                    // Report the deadline to the event loop so a `Parked`
                    // round suspends until it instead of hot-spinning.
                    cx.set_poll_hint(t);
                    return Ok(JobState::Again);
                }
                Some(_) => {}
            }

            // At least one timer is due: fire every elapsed timer in deadline
            // order.  `entries` is sorted, so `entries[0]` is always the nearest;
            // each poll fires all due timers in order (catch-up).
            loop {
                // Peek the nearest timer and, if due, pull it out.
                let step =
                    TIMER_STATE.with_borrow_mut(|opt| -> Option<(Object, Option<u64>, u32)> {
                        let s = opt.as_mut()?;
                        let e = s.entries.first_mut()?;
                        if e.deadline > now {
                            return None;
                        }
                        let cb = e.callback;
                        let id = e.id;
                        match e.interval_ms {
                            // One-shot: drop it now (before the callback, so a
                            // clearTimeout inside it is a no-op). The rest stay
                            // sorted.
                            None => {
                                s.entries.remove(0);
                                Some((cb, None, id))
                            }
                            // Interval: keep the entry; re-arm happens *after*
                            // the callback, since it may clear itself.
                            Some(ms) => Some((cb, Some(ms), id)),
                        }
                    });

                let (cb, interval_ms, id) = match step {
                    Some(x) => x,
                    None => break,
                };

                if cb.call(cx, JSValue::UNDEFINED, &[]).is_err() {
                    // Take the exception FIRST so cur_exception is cleared before we
                    // run any more JS (to_text below executes JS to format the message).
                    // Leaving the exception set while calling to_text() would let that
                    // inner call trip zhuque's "call returned Ok but left an exception
                    // set" debug_assert. The error is reported and then discarded — a
                    // timer callback throwing must not reject/crash the event loop.
                    let _exc = cx.take_exception();

                    #[cfg(debug_assertions)]
                    {
                        let msg = _exc.to_text(cx);
                        eprintln!("timer callback error: {}", msg.message);
                    }
                }

                if let Some(ms) = interval_ms {
                    // Re-arm only if the callback didn't clear this timer.
                    // Pull the entry out, bump its deadline, and re-insert in
                    // sorted order so `entries[0]` stays the nearest.
                    let new_deadline = Instant::now() + Duration::from_millis(ms);
                    TIMER_STATE.with_borrow_mut(|opt| {
                        if let Some(s) = opt.as_mut()
                            && let Some(pos) = s.entries.iter().position(|e| e.id == id)
                        {
                            let mut e = s.entries.remove(pos);
                            e.deadline = new_deadline;
                            s.insert_sorted(e);
                        }
                    });
                }
            }

            // ES §9.5: a handler must not leave a pending exception behind.
            if let Exception::Error(_) = cx.take_exception() {
                // already reported above; just discard.
            }

            let has_entries =
                TIMER_STATE.with_borrow(|s| s.as_ref().is_some_and(|s| !s.entries.is_empty()));
            if has_entries {
                // All due timers fired above, so `entries[0]` is in the
                // future: report it so the parked loop wakes exactly then.
                if let Some(d) = TIMER_STATE.with_borrow(|s| {
                    s.as_ref()
                        .and_then(|s| s.entries.first())
                        .map(|e| e.deadline)
                }) {
                    if d > now {
                        cx.set_poll_hint(d);
                    }
                }
                Ok(JobState::Again)
            } else {
                TIMER_STATE.with_borrow_mut(|s| {
                    s.take();
                });
                Ok(JobState::Done)
            }
        },
        (),
    );
}

// ---------------------------------------------------------------------------
// Register timer: shared logic for setTimeout / setInterval
// ---------------------------------------------------------------------------

fn register_timer(ctx: &mut ScopedContext, cc: CallContext, is_interval: bool) -> JSResult {
    let callback = cc.arg(0).ensure_callable(ctx)?;
    let delay_ms = if cc.length() >= 2 {
        cc.arg(1).to_number(ctx)?.to_i32().max(0) as u64
    } else {
        0
    };

    // If no active job, initialize fresh state and prepare to start one.
    // If already active, reuse the existing state (don't start a second job).
    let (id, need_listener) = TIMER_STATE.with_borrow_mut(|opt| {
        let start_job = opt.is_none();
        let s = opt.get_or_insert_with(TimerState::new);
        let id = s.timer_id();
        // Capture the absolute deadline NOW, synchronously. The `entries` vec is
        // kept sorted by this, so it is the single source of firing-order truth.
        let deadline = Instant::now() + Duration::from_millis(delay_ms);
        s.insert_sorted(TimerEntry {
            id,
            callback,
            interval_ms: if is_interval { Some(delay_ms) } else { None },
            deadline,
        });
        (id, start_job)
    });

    if need_listener {
        start_timer_task(ctx);
    }

    Ok(JSValue::from(id as i32))
}

// ---------------------------------------------------------------------------
// Clear timer: shared logic for clearTimeout / clearInterval
// ---------------------------------------------------------------------------

fn clear_timer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id = cc.arg(0).to_number(ctx)?.to_i32();
    if id <= 0 {
        return Ok(JSValue::UNDEFINED);
    }
    let id = id as u32;

    // Removing the entry is the entire cancellation: the self-requeuing job only
    // fires timers still in `entries`, so a cleared timer is never fired. The
    // rest of the vec stays sorted. No per-timer task needs aborting.
    TIMER_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut()
            && let Some(pos) = s.entries.iter().position(|e| e.id == id)
        {
            s.entries.remove(pos);
        }
    });

    Ok(JSValue::UNDEFINED)
}

/// `Aike.sleep(ms)` — returns a `Promise` that resolves after `ms` milliseconds.
///
/// Analogous to `Bun.sleep(ms)`. Implemented by creating a `Promise` and using
/// its native resolve function directly as a one-shot timer callback: when the
/// timer fires, calling resolve() settles the `Promise` with `undefined`. This
/// reuses the exact same timer queue as `setTimeout`/`setInterval`, so sleep is
/// driven by the event loop and never blocks the thread.
pub(crate) fn js_sleep(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let ms = cc.arg(0).to_number(ctx)?.to_f64();
    let delay_ms = if ms.is_nan() || ms < 0.0 {
        0
    } else {
        ms as u64
    };

    // Build the Promise and grab its native resolve function. The resolve fn is
    // callable, so it can serve directly as the timer callback.
    let (promise, resolving) = Promise::create(ctx)?;
    let resolve_fn = resolving[0].ensure_callable(ctx)?;

    let (_id, need_listener) = TIMER_STATE.with_borrow_mut(|opt| {
        let start_job = opt.is_none();
        let s = opt.get_or_insert_with(TimerState::new);
        let id = s.timer_id();
        let deadline = Instant::now() + Duration::from_millis(delay_ms);
        s.insert_sorted(TimerEntry {
            id,
            callback: resolve_fn,
            interval_ms: None,
            deadline,
        });
        (id, start_job)
    });

    if need_listener {
        start_timer_task(ctx);
    }

    Ok(promise.into())
}

const MAGIC_SET_TIMEOUT: i16 = 0;
const MAGIC_SET_INTERVAL: i16 = 1;
const MAGIC_CLEAR: i16 = 2; // clearTimeout and clearInterval share the same impl

fn js_timeout(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    match cc.magic() {
        m if m == MAGIC_SET_TIMEOUT as i32 => register_timer(ctx, cc, false),
        m if m == MAGIC_SET_INTERVAL as i32 => register_timer(ctx, cc, true),
        _ => clear_timer(ctx, cc), // MAGIC_CLEAR
    }
}

pub(crate) fn install_timeout(ctx: &mut ScopedContext, mut global: Object) -> Result<(), i32> {
    let f = ctx.create_native_function(
        Callback::Func(js_timeout),
        "setTimeout",
        2,
        MAGIC_SET_TIMEOUT,
    )?;
    global.define_property_value_str(ctx, "setTimeout", f.into(), JS_PROP_C_W_E)?;

    let f = ctx.create_native_function(
        Callback::Func(js_timeout),
        "setInterval",
        2,
        MAGIC_SET_INTERVAL,
    )?;
    global.define_property_value_str(ctx, "setInterval", f.into(), JS_PROP_C_W_E)?;

    let f =
        ctx.create_native_function(Callback::Func(js_timeout), "clearTimeout", 1, MAGIC_CLEAR)?;
    global.define_property_value_str(ctx, "clearTimeout", f.into(), JS_PROP_C_W_E)?;

    let f =
        ctx.create_native_function(Callback::Func(js_timeout), "clearInterval", 1, MAGIC_CLEAR)?;
    global.define_property_value_str(ctx, "clearInterval", f.into(), JS_PROP_C_W_E)?;

    Ok(())
}
