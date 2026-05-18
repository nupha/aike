// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! setTimeout / setInterval / clearTimeout / clearInterval
//!
//! Design:
//!   - Each timer spawns a tokio task that sleeps for the given delay,
//!     then sends `TimerMsg::Fired(id)` on a crossbeam channel.
//!   - clearTimeout/clearInterval sends `TimerMsg::Cancelled(id)` instead.
//!   - A single persistent microtask (queue_job) is active as long as any
//!     timer registrations exist.  It polls the channel with try_recv every
//!     microtask turn and returns JobState::Again to stay alive, or
//!     JobState::Done when all entries are gone (sets TIMER_STATE back to None).
//!   - setTimeout/setInterval call start_poll_job only when TIMER_STATE is None,
//!     so at most one polling job runs at any time.  No tick handler needed.

use std::{cell::RefCell, collections::HashMap};

use crossbeam::channel::{Receiver, Sender};
use zuqe::{
    CallContext, Callback, JS_PROP_C_W_E, JSResult, JSValue, Object, ScopedContext,
    builtins::promise::JobState,
    gc_lite::{GcTrace, GcTraceCtx},
};

// ---------------------------------------------------------------------------
// Channel message: fired or cancelled
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum TimerMsg {
    Fired(u32),
    Cancelled(u32),
}

// ---------------------------------------------------------------------------
// Per-timer entry stored in the registry
// ---------------------------------------------------------------------------

struct TimerEntry {
    /// JS function callback — must be GC-traced
    callback: JSValue,
    /// None for setTimeout; Some(ms) for setInterval
    interval_ms: Option<u64>,
    /// tx clone used to re-send Fired for interval repetition
    tx: Sender<TimerMsg>,
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
        let (tx, rx) = crossbeam::channel::unbounded::<TimerMsg>();
        Self {
            next_id: 1,
            entries: HashMap::new(),
            tx,
            rx,
        }
    }

    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

thread_local! {
    // Some(_) = polling microtask is active; None = no background job running.
    static TIMER_STATE: RefCell<Option<TimerState>> = const { RefCell::new(None) };
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

fn spawn_timer(id: u32, delay_ms: u64, tx: Sender<TimerMsg>) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        // If the receiver is gone the timer was cleared; ignore the error.
        let _ = tx.send(TimerMsg::Fired(id));
    });
}

// ---------------------------------------------------------------------------
// The single persistent polling microtask
// ---------------------------------------------------------------------------

/// Enqueue the polling microtask.  Called only when TIMER_STATE is Some
/// (state already initialized by the caller).
fn start_timer_microtask(ctx: &mut ScopedContext) {
    #[cfg(debug_assertions)]
    TIMER_STATE.with_borrow_mut(|opt| {
        debug_assert!(opt.is_some());
    });

    ctx.queue_job(
        |cx| {
            let msg =
                TIMER_STATE.with_borrow(|opt| opt.as_ref().and_then(|s| s.rx.try_recv().ok()));
            match msg {
                None => {
                    // Channel empty — check if there are still live entries.
                    let still_any = TIMER_STATE
                        .with_borrow(|opt| opt.as_ref().map_or(false, |s| !s.entries.is_empty()));
                    if still_any {
                        Ok(JobState::Again)
                    } else {
                        // All timers done; drop the state entirely.
                        TIMER_STATE.with_borrow_mut(|opt| *opt = None);
                        Ok(JobState::Done)
                    }
                }
                Some(TimerMsg::Cancelled(id)) => {
                    TIMER_STATE.with_borrow_mut(|opt| {
                        if let Some(s) = opt.as_mut() {
                            s.entries.remove(&id);
                        }
                    });
                    Ok(JobState::Again)
                }
                Some(TimerMsg::Fired(id)) => {
                    let (callback, interval_ms, tx) = TIMER_STATE.with_borrow_mut(|opt| {
                        if let Some(s) = opt.as_mut() {
                            if let Some(e) = s.entries.get(&id) {
                                return (Some(e.callback), e.interval_ms, Some(e.tx.clone()));
                            }
                        }
                        (None, None, None)
                    });

                    if let (Some(cb), tx_opt) = (callback, tx) {
                        if cb.call(cx, JSValue::UNDEFINED, &[]).is_err() {
                            let msg = cx.rt().exception().to_text(cx);
                            eprintln!("timer callback error: {}", msg.message);
                        }

                        match interval_ms {
                            Some(ms) => {
                                // Re-arm only if not cleared inside the callback
                                let still_alive = TIMER_STATE.with_borrow(|opt| {
                                    opt.as_ref().map_or(false, |s| s.entries.contains_key(&id))
                                });
                                if still_alive {
                                    if let Some(tx) = tx_opt {
                                        spawn_timer(id, ms, tx);
                                    }
                                }
                            }
                            None => {
                                TIMER_STATE.with_borrow_mut(|opt| {
                                    if let Some(s) = opt.as_mut() {
                                        s.entries.remove(&id);
                                    }
                                });
                            }
                        }
                    }

                    Ok(JobState::Again)
                }
            }
        },
        (),
    );
}

// ---------------------------------------------------------------------------
// Register timer: shared logic for setTimeout / setInterval
// ---------------------------------------------------------------------------

fn register_timer(ctx: &mut ScopedContext, cc: CallContext, is_interval: bool) -> JSResult {
    let func = cc.arg(0);
    func.ensure_function(ctx)?;

    let delay_ms = if cc.length() >= 2 {
        cc.arg(1).to_number(ctx)?.to_i32().max(0) as u64
    } else {
        0
    };

    // If no active job, initialize fresh state and prepare to start one.
    // If already active, reuse the existing tx from the live state.
    let (id, tx, need_job) = TIMER_STATE.with_borrow_mut(|opt| {
        let start_job = opt.is_none();
        let s = opt.get_or_insert_with(TimerState::new);
        let id = s.alloc_id();
        let tx = s.tx.clone();
        s.entries.insert(
            id,
            TimerEntry {
                callback: func,
                interval_ms: if is_interval { Some(delay_ms) } else { None },
                tx: tx.clone(),
            },
        );
        (id, tx, start_job)
    });

    spawn_timer(id, delay_ms, tx);

    if need_job {
        start_timer_microtask(ctx);
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
        if let Some(s) = opt.as_mut() {
            if s.entries.remove(&id).is_some() {
                let _ = s.tx.send(TimerMsg::Cancelled(id));
            }
        }
    });

    Ok(JSValue::UNDEFINED)
}

// magic() values for js_timer
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
