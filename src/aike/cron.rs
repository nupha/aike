// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::RefCell, collections::HashMap, str::FromStr, time::Duration};

use chrono::Utc;
use cron::Schedule;
use crossbeam::channel::{Receiver, Sender};
use zuqe::{
    CallContext, Callback, JS_PROP_C_W_E, JSResult, JSValue, Object, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

// ---------------------------------------------------------------------------
// Channel message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum CronMsg {
    Fired(u32),
    Cancelled(u32),
}

// ---------------------------------------------------------------------------
// Per-job entry stored in the registry
// ---------------------------------------------------------------------------

struct CronEntry {
    name: Option<String>,
    pattern: String,
    schedule: Schedule,
    callback: Object,
    job_obj: Object,
    active: bool,
}

// ---------------------------------------------------------------------------
// Thread-local global state
// ---------------------------------------------------------------------------

struct CronState {
    next_id: u32,
    entries: HashMap<u32, CronEntry>,
    tx: Sender<CronMsg>,
    rx: Receiver<CronMsg>,
}

impl CronState {
    fn new() -> Self {
        let (tx, rx) = crossbeam::channel::bounded::<CronMsg>(64);
        Self {
            next_id: 1,
            entries: HashMap::new(),
            tx,
            rx,
        }
    }
}

thread_local! {
    static CRON_STATE: RefCell<Option<CronState>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// GC trace — keep all live callbacks and job objects alive
// ---------------------------------------------------------------------------

pub(crate) fn gc_trace_cron(tr: &mut GcTraceCtx) {
    CRON_STATE.with_borrow(|opt| {
        if let Some(s) = opt.as_ref() {
            for entry in s.entries.values() {
                entry.callback.trace(tr);
                entry.job_obj.trace(tr);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Spawn a tokio task that sleeps until next fire time
// ---------------------------------------------------------------------------

fn spawn_cron_timer(id: u32, schedule: &Schedule) {
    // Clone tx on the JS thread before moving into tokio worker
    let tx = CRON_STATE.with_borrow(|opt| opt.as_ref().map(|s| s.tx.clone()));
    let Some(tx) = tx else { return };

    let next = schedule.upcoming(Utc).next();
    let Some(next_time) = next else {
        // No more future occurrences — auto-expire
        let _ = tx.send(CronMsg::Cancelled(id));
        return;
    };

    let delay_ms = (next_time.timestamp() - Utc::now().timestamp()).max(0) as u64;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let _ = tx.send(CronMsg::Fired(id));
    });
}

// ---------------------------------------------------------------------------
// The persistent polling microtask
// ---------------------------------------------------------------------------

fn add_cron_notifier(ctx: &mut ScopedContext) {
    let rx = CRON_STATE.with_borrow(|opt| opt.as_ref().unwrap().rx.clone());

    crate::helpers::start_channel_poller(
        ctx,
        rx,
        |msg, cx| {
            match msg {
                CronMsg::Cancelled(id) => {
                    CRON_STATE.with_borrow_mut(|opt| {
                        if let Some(s) = opt.as_mut() {
                            s.entries.remove(&id);
                        }
                    });
                }
                CronMsg::Fired(id) => {
                    // Extract callback and job object, check active
                    let fire_info = CRON_STATE.with_borrow_mut(|opt| {
                        let s = opt.as_mut()?;
                        let e = s.entries.get_mut(&id)?;
                        if !e.active {
                            return None;
                        }
                        Some((e.callback, e.job_obj))
                    });

                    if let Some((callback, mut job_obj)) = fire_info {
                        // Invoke JS callback
                        if callback.call(cx, JSValue::UNDEFINED, &[]).is_err() {
                            let msg = cx.rt().exception().to_text(cx);
                            eprintln!("cron callback error: {}", msg.message);
                        }

                        // Re-schedule if still active
                        let still_active = CRON_STATE.with_borrow(|opt| {
                            opt.as_ref()
                                .and_then(|s| s.entries.get(&id))
                                .map(|e| e.active)
                                .unwrap_or(false)
                        });

                        if still_active {
                            CRON_STATE.with_borrow(|opt| {
                                if let Some(s) = opt.as_ref()
                                    && let Some(e) = s.entries.get(&id)
                                {
                                    spawn_cron_timer(id, &e.schedule);
                                }
                            });

                            // Update nextRun on job object
                            if let Some(next_iso) = compute_next_run_iso(id) {
                                let _ = cx.create_string_from_utf8(next_iso.into_bytes()).and_then(
                                    |s| {
                                        job_obj.define_property_value_str(
                                            cx,
                                            "nextRun",
                                            s.into(),
                                            JS_PROP_C_W_E,
                                        )
                                    },
                                );
                            }
                        } else {
                            // Mark inactive on job object
                            let _ = job_obj.define_property_value_str(
                                cx,
                                "active",
                                JSValue::Bool(false),
                                JS_PROP_C_W_E,
                            );
                        }
                    }
                }
            }
            true // always continue polling
        },
        || CRON_STATE.with_borrow(|s| s.as_ref().is_some_and(|s| !s.entries.is_empty())),
        || {
            CRON_STATE.with_borrow_mut(|s| {
                s.take();
            })
        },
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compute the next fire time ISO string for a cron entry.
fn compute_next_run_iso(id: u32) -> Option<String> {
    CRON_STATE.with_borrow(|opt| {
        let s = opt.as_ref()?;
        let e = s.entries.get(&id)?;
        e.schedule
            .upcoming(Utc)
            .next()
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
    })
}

/// Normalize a 5-field standard cron expression to the 7-field format
/// expected by the `cron` crate: `sec min hour dom month dow year`.
fn normalize_cron_expr(expr: &str) -> String {
    let fields: Vec<&str> = expr.trim().split_whitespace().collect();
    match fields.len() {
        5 => format!("0 {} *", expr.trim()), // prepend seconds, append year
        6 => format!("{} *", expr.trim()),   // has seconds, append year
        _ => expr.trim().to_string(),        // assume already 7-field
    }
}

// ---------------------------------------------------------------------------
// JS API: Aike.schedule(pattern, callback, options?)
// ---------------------------------------------------------------------------

pub(super) fn js_schedule(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // arg 0: cron expression string
    let pattern = cc.arg(0).to_string(ctx)?;
    let pattern_str = pattern.content().encode_utf8(false).to_string();

    // arg 1: callback function
    let callback = cc.arg(1).into_callable(ctx)?;

    // arg 2 (optional): options { name?: string }
    let name: Option<String> = if cc.length() > 2 {
        cc.arg(2).as_object().and_then(|opts| {
            let v = opts.get_property_str(ctx, "name").ok()?;
            let s = v.as_string()?;
            Some(s.content().encode_utf8(false).to_string())
        })
    } else {
        None
    };

    // Parse cron expression
    let normalized = normalize_cron_expr(&pattern_str);
    let schedule = Schedule::from_str(&normalized).map_err(|e| {
        ctx.throw_internal_error(&format!("invalid cron expression '{}': {}", pattern_str, e));
        -1
    })?;

    // Initialize state if needed
    let (id, tx, need_notifier) = CRON_STATE.with_borrow_mut(|opt| {
        let start_job = opt.is_none();
        let s = opt.get_or_insert_with(CronState::new);
        let id = s.next_id;
        s.next_id += 1;
        let tx = s.tx.clone();
        (id, tx, start_job)
    });

    // Compute nextRun before creating the job object
    let next_run_iso = schedule
        .upcoming(Utc)
        .next()
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string());

    // Build JS job object — pre-create all string values to avoid double-borrow of ctx
    let mut job = ctx.create_base_object()?;

    let name_val: JSValue = match &name {
        Some(n) => ctx.create_string_from_utf8(n.clone().into_bytes())?.into(),
        None => JSValue::NULL,
    };
    let pattern_val = ctx
        .create_string_from_utf8(pattern_str.clone().into_bytes())?
        .into();
    let next_run_val: JSValue = match next_run_iso {
        Some(ref iso) => ctx
            .create_string_from_utf8(iso.clone().into_bytes())?
            .into(),
        None => JSValue::NULL,
    };
    let cancel_fn = ctx
        .create_native_function(Callback::Func(js_job_cancel), "cancel", 0, 0)?
        .into();

    job.define_property_value_str(ctx, "id", JSValue::from(id as i32), JS_PROP_C_W_E)?;
    job.define_property_value_str(ctx, "name", name_val, JS_PROP_C_W_E)?;
    job.define_property_value_str(ctx, "pattern", pattern_val, JS_PROP_C_W_E)?;
    job.define_property_value_str(ctx, "nextRun", next_run_val, JS_PROP_C_W_E)?;
    job.define_property_value_str(ctx, "active", JSValue::Bool(true), JS_PROP_C_W_E)?;
    job.define_property_value_str(ctx, "cancel", cancel_fn, JS_PROP_C_W_E)?;

    // Store entry (clone job_obj so cancel can update it)
    CRON_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut() {
            s.entries.insert(
                id,
                CronEntry {
                    name,
                    pattern: pattern_str,
                    schedule: schedule.clone(),
                    callback,
                    job_obj: job.clone(),
                    active: true,
                },
            );
        }
    });

    // Spawn first timer
    spawn_cron_timer(id, &schedule);

    // Start poller if this is the first job
    if need_notifier {
        add_cron_notifier(ctx);
    }

    // Suppress unused warning — tx is cloned into spawn_cron_timer via CRON_STATE
    let _ = tx;

    Ok(job.into())
}

// ---------------------------------------------------------------------------
// JS API: job.cancel()
// ---------------------------------------------------------------------------

fn js_job_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let id = this
        .as_object()
        .and_then(|obj| obj.get_property_str(ctx, "id").ok())
        .and_then(|v| v.to_number(ctx).ok().map(|n| n.to_i32()))
        .unwrap_or(-1);

    if id <= 0 {
        return Ok(JSValue::UNDEFINED);
    }
    let id = id as u32;

    // Mark inactive and remove from state
    let was_active = CRON_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut() {
            if let Some(entry) = s.entries.get_mut(&id) {
                entry.active = false;
            }
            // Send cancel to interrupt any pending sleep
            let _ = s.tx.send(CronMsg::Cancelled(id));
            s.entries.remove(&id).is_some()
        } else {
            false
        }
    });

    if was_active {
        // Update active flag on the JS job object
        if let Some(mut obj) = this.as_object().cloned() {
            let _ =
                obj.define_property_value_str(ctx, "active", JSValue::Bool(false), JS_PROP_C_W_E);
        }
    }

    Ok(JSValue::UNDEFINED)
}
