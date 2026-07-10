// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Timers Standard: setTimeout, setInterval, clearTimeout, clearInterval.
//!
//! Uses a crossbeam channel + tokio async sleep for background timer dispatch,
//! and a single persistent JS job (via start_channel_poller) to invoke callbacks.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    time::{Duration, Instant},
};

use crossbeam::channel::{Receiver, Sender};
use zuqe::{
    CallContext, Callback, Exception, JS_PROP_C_W_E, JSResult, JSValue, JobState, Object,
    ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

// ---------------------------------------------------------------------------
// Channel message: fired or cancelled
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum TimerMsg {
    /// Wake-up signal: a per-timer tokio task reached its deadline. The poller
    /// decides the actual firing order from each entry's absolute `deadline`,
    /// NOT from the order these wake-ups arrive (which races under the async
    /// runtime). `Fired` carries no data — it is only a wake-up signal; the
    /// poller decides what to fire from each entry's absolute `deadline`.
    Fired,
    Cancelled(u32),
}

// ---------------------------------------------------------------------------
// Per-timer entry stored in the registry
// ---------------------------------------------------------------------------

struct TimerEntry {
    /// JS function callback — must be GC-traced
    callback: Object,
    /// None for setTimeout; Some(ms) for setInterval
    interval_ms: Option<u64>,
    /// tx clone used to re-send Fired for interval repetition
    tx: Sender<TimerMsg>,
    /// Absolute deadline captured at registration (and on every re-arm for
    /// intervals). This — and only this — determines firing order, so the
    /// result is deterministic regardless of async wake-up races.
    deadline: Instant,
}

// ---------------------------------------------------------------------------
// Thread-local global state
// ---------------------------------------------------------------------------

struct TimerState {
    next_id: u32,
    entries: HashMap<u32, TimerEntry>,
    tx: Sender<TimerMsg>,
    rx: Receiver<TimerMsg>,
}

impl TimerState {
    fn new() -> Self {
        let (tx, rx) = crossbeam::channel::bounded::<TimerMsg>(64);
        let next_id = TIMER_SERIAL.with(|t| {
            let id = t.get();
            t.set(id + 1);
            id
        });
        Self {
            next_id,
            entries: HashMap::new(),
            tx,
            rx,
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
            for entry in s.entries.values() {
                entry.callback.trace(tr);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Spawn a tokio task that sleeps then fires a message on the channel
// ---------------------------------------------------------------------------

/// Spawn a tokio task that sleeps until `deadline` and then sends a `Fired`
/// wake-up signal. The deadline is captured by the caller (so it matches the
/// value stored in the entry) and passed in, keeping the sleep and the
/// registry in sync. `Fired` carries no payload — the poller scans all entries
/// by deadline, so the id is not needed here.
fn spawn_timer(deadline: Instant, tx: Sender<TimerMsg>) {
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        let _ = tx.send(TimerMsg::Fired);
    });
}

// ---------------------------------------------------------------------------
// The single persistent polling microtask
// ---------------------------------------------------------------------------

/// Enqueue the polling microtask.  Called only when TIMER_STATE is Some
/// (state already initialized by the caller).
///
/// The poller drains every message that is currently ready and fires the
/// `Fired` callbacks in **deadline order**, so multiple timers that become
/// due in the same poll window still execute in the order of their requested
/// delay (rather than in the arbitrary wake order of the per-timer tokio
/// tasks).  `Cancelled` messages are applied first so an already-in-flight
/// `Fired` for a cleared id is dropped instead of invoking its callback.
fn add_timeout_notifier(ctx: &mut ScopedContext) {
    #[cfg(debug_assertions)]
    TIMER_STATE.with_borrow_mut(|opt| {
        debug_assert!(opt.is_some());
    });

    let rx = TIMER_STATE.with_borrow(|opt| opt.as_ref().unwrap().rx.clone());

    ctx.queue_job(
        move |cx| {
            // 1. Drain every wake-up currently ready. `Fired` is only a signal
            //    that *some* deadline passed; the real firing order is decided
            //    by each entry's absolute `deadline` below, which is immune to
            //    the async wake-up race. `Cancelled` removes the entry.
            let mut cancelled: Vec<u32> = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(TimerMsg::Cancelled(id)) => {
                        cancelled.push(id);
                    }
                    Ok(TimerMsg::Fired) => {}
                    Err(_) => break,
                }
            }

            // Fire every entry whose deadline has been reached, in ascending
            // deadline order. This is deterministic and correct even when the
            // underlying tokio tasks delivered their `Fired` signals out of
            // order (a later-deadline timer can wake before an earlier one).
            // Cancelled entries are dropped here too, inside the same borrow so
            // we don't re-enter TIMER_STATE once per cancelled id.
            let now = Instant::now();
            let mut due: Vec<(Instant, u32)> = TIMER_STATE.with_borrow_mut(|opt| {
                let mut v = Vec::new();
                if let Some(s) = opt.as_mut() {
                    for id in &cancelled {
                        s.entries.remove(id);
                    }

                    for (id, e) in s.entries.iter() {
                        if e.deadline <= now {
                            v.push((e.deadline, *id));
                        }
                    }
                }
                v
            });

            due.sort_by_key(|entry| entry.0);

            for (_, id) in due {
                // Extract the callback and settle this timer's fate in ONE borrow.
                // A one-shot (setTimeout, interval_ms == None) is removed
                // immediately, so no second borrow is needed after the callback.
                // An interval keeps its entry and hands back its re-arm data —
                // its liveness (whether the callback cleared it) is re-checked
                // *after* cb.call below, since the callback may re-enter
                // TIMER_STATE and we must not hold a borrow across it.
                let (callback, re_arm) = TIMER_STATE.with_borrow_mut(|opt| {
                    if let Some(s) = opt.as_mut()
                        && let Some(e) = s.entries.get_mut(&id)
                    {
                        let cb = e.callback;
                        match e.interval_ms {
                            None => {
                                s.entries.remove(&id);
                                (Some(cb), None)
                            }
                            Some(ms) => (Some(cb), Some((ms, e.tx.clone()))),
                        }
                    } else {
                        (None, None)
                    }
                });

                if let Some(cb) = callback {
                    if cb.call(cx, JSValue::UNDEFINED, &[]).is_err() {
                        let msg = cx.rt().exception().to_text(cx);
                        eprintln!("timer callback error: {}", msg.message);
                        let _ = cx.take_exception();
                    }

                    if let Some((ms, tx)) = re_arm {
                        // Re-arm only if not cleared inside the callback.
                        // The liveness check and the deadline update are done in
                        // a single borrow (a live entry is exactly one that
                        // get_mut finds), avoiding a redundant with_borrow.
                        let new_deadline = Instant::now() + Duration::from_millis(ms);
                        let keep = TIMER_STATE.with_borrow_mut(|opt| {
                            if let Some(s) = opt.as_mut()
                                && let Some(e) = s.entries.get_mut(&id)
                            {
                                e.deadline = new_deadline;
                                true
                            } else {
                                false
                            }
                        });
                        if keep {
                            spawn_timer(new_deadline, tx);
                        }
                    }
                }
            }

            // ES §9.5: a handler must not leave a pending exception behind.
            if let Exception::Error(_) = cx.take_exception() {
                // already reported above; just discard.
            }

            let has_entries =
                TIMER_STATE.with_borrow(|s| s.as_ref().is_some_and(|s| !s.entries.is_empty()));
            if has_entries {
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
    let callback = cc.arg(0).into_callable(ctx)?;
    let delay_ms = if cc.length() >= 2 {
        cc.arg(1).to_number(ctx)?.to_i32().max(0) as u64
    } else {
        0
    };

    // If no active job, initialize fresh state and prepare to start one.
    // If already active, reuse the existing tx from the live state.
    let (id, tx, deadline, need_listener) = TIMER_STATE.with_borrow_mut(|opt| {
        let start_job = opt.is_none();
        let s = opt.get_or_insert_with(TimerState::new);
        let id = s.timer_id();
        let tx = s.tx.clone();
        // Capture the absolute deadline NOW, synchronously, so it is identical
        // for the spawned sleep task and the registry entry (the single source
        // of firing-order truth).
        let deadline = Instant::now() + Duration::from_millis(delay_ms);
        s.entries.insert(
            id,
            TimerEntry {
                callback,
                interval_ms: if is_interval { Some(delay_ms) } else { None },
                tx: tx.clone(),
                deadline,
            },
        );
        (id, tx, deadline, start_job)
    });

    spawn_timer(deadline, tx.clone());

    if need_listener {
        add_timeout_notifier(ctx);
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

    TIMER_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut()
            && s.entries.remove(&id).is_some()
        {
            let _ = s.tx.send(TimerMsg::Cancelled(id));
        }
    });

    Ok(JSValue::UNDEFINED)
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
