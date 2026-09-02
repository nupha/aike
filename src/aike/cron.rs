// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    collections::HashMap,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::Utc;
use cron::Schedule;
use crossbeam::channel::{Receiver, Sender};
use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassID, FieldDef, JS_PROP_C_W_E, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

#[derive(Debug, Clone, Copy)]
enum CronMsg {
    Fired(u32),
    Cancelled(u32),
}

struct CronEntry {
    schedule: Schedule,
    callback: Object,
    job_obj: Object, // native JSCronJob instance — for updating nextRun
    active: bool,
}

struct CronState {
    next_id: u32,
    entries: HashMap<u32, CronEntry>,
    tx: zuqe::WakeSender<CronMsg>,
    rx: Receiver<CronMsg>,
}

impl CronState {
    fn new(wake: zuqe::WakeHandle) -> Self {
        let (tx, rx) = crossbeam::channel::bounded::<CronMsg>(64);
        Self {
            next_id: 1,
            entries: HashMap::new(),
            tx: zuqe::WakeSender::wrap(tx, wake),
            rx,
        }
    }
}

thread_local! {
    static CRON_STATE: RefCell<Option<CronState>> = const { RefCell::new(None) };
}

/// GC trace — keep all live callbacks and job objects alive
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

/// Compute delay in milliseconds to the next cron fire time.
fn next_delay_ms(schedule: &Schedule) -> Option<u64> {
    schedule
        .upcoming(Utc)
        .next()
        .map(|next| (next.timestamp() - Utc::now().timestamp()).max(0) as u64)
}

/// Format the next fire time as ISO 8601 UTC string.
fn format_next_run(schedule: &Schedule) -> Option<String> {
    schedule
        .upcoming(Utc)
        .next()
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
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

/// Spawn a tokio task that sleeps until next fire time
fn spawn_cron_timer(id: u32, schedule: &Schedule) {
    // Clone the (wake-carrying) tx on the JS thread before moving into the
    // tokio worker; sends wake the parked loop automatically.
    let tx = CRON_STATE.with_borrow(|opt| opt.as_ref().map(|s| s.tx.clone()));
    let Some(tx) = tx else { return };

    let Some(delay_ms) = next_delay_ms(schedule) else {
        // No more future occurrences — auto-expire
        let _ = tx.send(CronMsg::Cancelled(id));
        return;
    };

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let _ = tx.send(CronMsg::Fired(id));
    });
}

/// The persistent polling microtask
fn add_cron_notifier(ctx: &mut ScopedContext) {
    let rx = CRON_STATE.with_borrow(|opt| opt.as_ref().unwrap().rx.clone());

    crate::helpers::start_channel_poller(
        ctx,
        "cron",
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
                            let next_iso = CRON_STATE.with_borrow(|opt| {
                                opt.as_ref()
                                    .and_then(|s| s.entries.get(&id))
                                    .and_then(|e| format_next_run(&e.schedule))
                            });
                            if let Some(iso) = next_iso {
                                let _ =
                                    cx.create_string_from_utf8(iso.into_bytes()).and_then(|s| {
                                        job_obj.define_property_value_str(
                                            cx,
                                            "nextRun",
                                            JSValue::make_string(s),
                                            JS_PROP_C_W_E,
                                        )
                                    });
                            }
                        } else {
                            // Mark inactive on job object
                            let _ = job_obj.define_property_value_str(
                                cx,
                                "active",
                                JSValue::FALSE,
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
// JSCronJob native class
// ---------------------------------------------------------------------------

struct CronJobPayload {
    id: u32,
}

impl CronJobPayload {
    #[inline(always)]
    fn gc_trace(&self, _gcx: &mut GcTraceCtx) {
        // No JS objects in payload — nothing to trace
    }
}

impl zuqe::gc_lite::GcTrace for CronJobPayload {
    fn trace(&self, gcx: &mut zuqe::gc_lite::GcTraceCtx) {
        self.gc_trace(gcx);
    }
}

fn get_cron_job<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a CronJobPayload, i32> {
    JSCronJob::payload_of(val).ok_or_else(|| ctx.throw_internal_error("not a CronJob instance"))
}

/// Prototype method: job.cancel()
fn js_cron_job_cancel(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_cron_job(ctx, &this)?;
    let id = payload.id;

    if id == 0 {
        return Ok(JSValue::UNDEFINED);
    }

    // Remove from state and cancel pending timer
    CRON_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut() {
            if let Some(entry) = s.entries.get_mut(&id) {
                entry.active = false;
            }
            let _ = s.tx.send(CronMsg::Cancelled(id));
            s.entries.remove(&id);
        }
    });

    // Update active property on the JS object
    if let Some(mut obj) = cc.this().as_object() {
        let _ = obj.define_property_value_str(ctx, "active", JSValue::FALSE, JS_PROP_C_W_E);
    }

    Ok(JSValue::UNDEFINED)
}

const CRON_JOB_PROTO: &[FieldDef] = &[FieldDef::FUNC("cancel", 0, js_cron_job_cancel, 0)];

def_native_object!(
    "CronJob",
    JSCronJob,
    CronJobPayload,
    CRON_JOB_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_cron_job(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSCronJob::register(rt)
}

/// JS API: Aike.schedule(pattern, callback, options?)
pub(super) fn js_schedule(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // arg 0: cron expression string
    let pattern = cc.arg(0).to_string(ctx)?;
    let pattern_str = pattern.content().encode_utf8(false).to_string();

    // arg 1: callback function
    let callback = cc.arg(1).ensure_callable(ctx)?;

    // arg 2 (optional): options { name?: string }
    let name: Option<String> = if cc.length() > 2 {
        cc.arg(2).as_object().and_then(|opts| {
            let v = opts.get_property_str(ctx, "name").ok()?;
            let s = v.as_string(true)?;
            Some(s.content().encode_utf8(false).to_string())
        })
    } else {
        None
    };

    // Parse cron expression
    let normalized = normalize_cron_expr(&pattern_str);
    let schedule = Schedule::from_str(&normalized).map_err(|e| {
        ctx.throw_internal_error(&format!("invalid cron expression '{}': {}", pattern_str, e))
    })?;

    // Initialize state if needed
    let (id, need_notifier) = CRON_STATE.with_borrow_mut(|opt| {
        let start_job = opt.is_none();
        let s = opt.get_or_insert_with(|| CronState::new(ctx.wake_handle()));
        let id = s.next_id;
        s.next_id += 1;
        (id, start_job)
    });

    // Compute nextRun
    let next_run_val: JSValue = match format_next_run(&schedule) {
        Some(iso) => ctx
            .create_string_from_utf8(iso.into_bytes())
            .map(JSValue::make_string)?,
        None => JSValue::NULL,
    };

    // Pre-create string values to avoid double-borrow of ctx
    let name_val: JSValue = match &name {
        Some(n) => ctx
            .create_string_from_utf8(n.clone().into_bytes())
            .map(JSValue::make_string)?,
        None => JSValue::NULL,
    };
    let pattern_val = ctx
        .create_string_from_utf8(pattern_str.clone().into_bytes())
        .map(JSValue::make_string)?;

    // Create native JSCronJob instance
    let job_instance = JSCronJob::new_instance(ctx, JSValue::UNDEFINED, CronJobPayload { id })?;
    let job_val: JSValue = job_instance.into();

    // Set data properties on the instance
    let mut job_obj = job_val
        .as_object()
        .ok_or_else(|| ctx.throw_internal_error("failed to create CronJob object"))?
        .clone();
    job_obj.define_property_value_str(ctx, "id", JSValue::from(id as i32), JS_PROP_C_W_E)?;
    job_obj.define_property_value_str(ctx, "name", name_val, JS_PROP_C_W_E)?;
    job_obj.define_property_value_str(ctx, "pattern", pattern_val, JS_PROP_C_W_E)?;
    job_obj.define_property_value_str(ctx, "nextRun", next_run_val, JS_PROP_C_W_E)?;
    job_obj.define_property_value_str(ctx, "active", JSValue::TRUE, JS_PROP_C_W_E)?;

    // Store entry
    CRON_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut() {
            s.entries.insert(
                id,
                CronEntry {
                    schedule: schedule.clone(),
                    callback,
                    job_obj: job_obj.clone(),
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

    Ok(job_val)
}

// ---------------------------------------------------------------------------
// Rust-side helper: schedule an async job on a cron pattern
// ---------------------------------------------------------------------------

/// Handle to cancel a Rust-side cron job.
#[allow(dead_code)]
pub(crate) struct CronHandle {
    active: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl CronHandle {
    /// Stop the scheduled job. Subsequent fires are suppressed.
    pub fn cancel(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// Schedule an async closure on a cron pattern (5 or 7 field).
///
/// Returns a [`CronHandle`] that can be used to cancel the job.
///
/// ```ignore
/// let handle = schedule_async("*/5 * * * *", || async {
///     println!("every 5 minutes");
/// }).unwrap();
///
/// // later...
/// handle.cancel();
/// ```
#[allow(dead_code)]
pub(crate) fn schedule_async<F, Fut>(pattern: &str, job: F) -> Result<CronHandle, String>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let normalized = normalize_cron_expr(pattern);
    let schedule = Schedule::from_str(&normalized)
        .map_err(|e| format!("invalid cron '{}': {}", pattern, e))?;

    let active = Arc::new(AtomicBool::new(true));
    let active_clone = active.clone();

    tokio::spawn(async move {
        loop {
            // Check cancelled before sleeping
            if !active_clone.load(Ordering::Acquire) {
                break;
            }

            // Compute delay to next fire
            let Some(delay_ms) = next_delay_ms(&schedule) else {
                break; // no more occurrences
            };
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            // Check cancelled after sleeping
            if !active_clone.load(Ordering::Acquire) {
                break;
            }

            // Run the job
            job().await;
        }
    });

    Ok(CronHandle { active })
}
