// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_io_handle;
mod cron;
mod download;
mod persistent;
pub(crate) mod pipe_bridge;
mod runtime;
pub(crate) mod sandbox;
mod signal;
mod spawn;
mod sys;

pub(crate) use {
    class_io_handle::{JSFileHandle, JSStdioHandle, gc_trace_io, make_stats},
    cron::{JSCronJob, gc_trace_cron},
    runtime::{
        ensure_root_runtime, register_runtime_traces, save_process_info, set_root_ctx,
        set_root_runtime,
    },
    signal::{install_default_signal_handlers, register_signal_handler, signal_number},
    spawn::{kill_all_children, terminate_process},
    sys::{get_time_ns, which_executable},
};

use std::{collections::HashMap, time::Instant};

use zuqe::{
    FieldDef, JS_PROP_ENUMERABLE, JSValue, Object, RuntimeUserState, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

/// Per-runtime host state, attached to every runtime (root and child) via
/// `Runtime::create_with` / `Runtime::create_child_with` and reached through
/// [`RuntimeImpl::user_state_mut`].
///
/// Host state that used to live in per-runtime thread-locals is consolidated
/// here so that two runtimes on the same thread never share it. The engine
/// traces the whole struct on every collection, replacing the hand-written
/// `gc_trace_*` hooks.
///
/// [`RuntimeImpl::user_state_mut`]: zuqe::RuntimeImpl::user_state_mut
pub(crate) struct AikeRuntimeState {
    /// Signal number -> JS callback (last `onSignal` wins). Root-only in
    /// practice (the `onSignal` API is not installed on children).
    pub(crate) signals: HashMap<u32, JSValue>,
    /// `process.nextTick` callbacks pending for the next microtask.
    pub(crate) next_tick: Vec<JSValue>,
    /// `Aike.evalScript` nesting depth.
    pub(crate) eval_script_depth: u32,

    /// `console.group` indentation level.
    pub(crate) console_group_indent: usize,
    /// `console.count(label)` counters.
    pub(crate) console_counters: HashMap<String, usize>,
    /// `console.time(label)` start instants.
    pub(crate) console_timers: HashMap<String, Instant>,

    /// Pending setTimeout/setInterval/Aike.sleep timers, sorted by deadline.
    pub(crate) timers: crate::whatwg::timeout::TimerState,
    /// Auto-increment timer id counter.
    pub(crate) timer_serial: u32,

    /// In-flight fetch abort signals kept alive across GC, id -> signal
    /// (ids start at 1; 0 is the "no signal" sentinel). The entry is removed
    /// once the fetch settles.
    pub(crate) fetch_signals: HashMap<usize, JSValue>,
    /// Auto-increment id for `fetch_signals`.
    pub(crate) fetch_signal_next_id: usize,

    /// Browser CDP callback registries (cert error, page interception/console).
    pub(crate) browser: crate::browser::BrowserState,

    /// stdio MCP server's JS handler objects, kept alive across GC.
    pub(crate) stdio_js_handlers: Vec<Object>,
}

impl GcTrace for AikeRuntimeState {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        for cb in self.signals.values() {
            cb.trace(gcx);
        }
        for cb in &self.next_tick {
            cb.trace(gcx);
        }
        for v in self.fetch_signals.values() {
            v.trace(gcx);
        }
        self.browser.trace(gcx);
        for h in &self.stdio_js_handlers {
            h.trace(gcx);
        }
        self.timers.trace(gcx);
    }
}

impl AikeRuntimeState {
    pub(crate) fn new() -> Self {
        Self {
            signals: HashMap::new(),
            next_tick: Vec::new(),
            eval_script_depth: 0,
            console_group_indent: 0,
            console_counters: HashMap::new(),
            console_timers: HashMap::new(),
            fetch_signals: HashMap::new(),
            fetch_signal_next_id: 1,
            browser: Default::default(),
            stdio_js_handlers: Vec::new(),
            timers: Default::default(),
            timer_serial: 1,
        }
    }

    /// Borrow this runtime's state from a live context. Keeps call sites terse
    /// (`AikeRuntimeState::of(ctx).field`); the borrow lasts as long as `ctx`.
    pub(crate) fn of<'a, 'c>(ctx: &'a mut ScopedContext<'c>) -> &'a mut Self {
        ctx.rt_mut().user_state_mut::<Self>()
    }
}

impl RuntimeUserState for AikeRuntimeState {}

/// Install the `Aike` namespace. Root-only entries — `Aike.onSignal`,
/// `Aike.schedule`, `Aike.ttyGetWinSize`, `Aike.ttySetRaw`, plus the host-only
/// code entry points (`Aike.createRuntime`, `Aike.evalScript`,
/// `Aike.evalModule`) — are installed only on a root runtime
/// (`ctx.rt().is_root()`). A child runtime never gets them, so no runtime
/// check is needed at call time. The parent tick handler that drives child
/// runtimes is likewise registered only on the root runtime.
pub(crate) fn install_aike_namespace(ctx: &mut ScopedContext) -> Result<(), i32> {
    let is_root_rt = ctx.rt().is_root();
    let mut ns = ctx.create_base_object()?;

    let mut entries: Vec<FieldDef> = vec![
        // --- spawn ---
        FieldDef::FUNC("spawnSync", 0, spawn::js_spawn, 0),
        FieldDef::FUNC("spawn", 0, spawn::js_spawn, 1),
        // --- sys ---
        FieldDef::FUNC("gc", 0, sys::js_gc, 0),
        FieldDef::FUNC("isatty", 1, sys::js_isatty, 0),
        FieldDef::FUNC("now", 0, sys::js_now, 0),
        FieldDef::FUNC("homedir", 0, sys::js_homedir, 0),
        FieldDef::FUNC("tmpdir", 0, sys::js_tmpdir, 0),
        FieldDef::FUNC("which", 1, sys::js_which, 0),
        FieldDef::FUNC("gzip", 1, crate::gzip::js_gzip, 0),
        FieldDef::FUNC("gunzip", 1, crate::gzip::js_gunzip, 0),
        FieldDef::FUNC("sh", 1, spawn::js_sh, 0),
        FieldDef::FUNC("download", 2, download::js_download, 0),
        FieldDef::LAZY_VALUE("signals", signal::js_signals, JS_PROP_ENUMERABLE),
        // --- timer ---
        FieldDef::FUNC("sleep", 0, crate::whatwg::timeout::js_sleep, 0),
        // --- persistent ---
        FieldDef::FUNC("dumpValue", 1, persistent::js_dump_value, 0),
        FieldDef::FUNC("restoreValue", 1, persistent::js_restore_value, 0),
    ];

    if is_root_rt {
        // Root-only APIs: process/thread-global (signals, terminal, cron).
        entries.push(FieldDef::FUNC("ttyGetWinSize", 1, sys::js_ttyGetWinSize, 0));
        entries.push(FieldDef::FUNC("ttySetRaw", 1, sys::js_ttySetRaw, 0));
        entries.push(FieldDef::FUNC("onSignal", 2, signal::js_on_signal, 0));
        entries.push(FieldDef::FUNC("schedule", 2, cron::js_schedule, 0));
        // Host-only code entry points.
        entries.push(FieldDef::FUNC("evalScript", 1, sys::js_evalScript, 0));
        entries.push(FieldDef::FUNC("evalModule", 2, sys::js_evalModule, 0));
        entries.push(FieldDef::FUNC(
            "createRuntime",
            1,
            runtime::js_create_runtime,
            0,
        ));
    }

    ns.install_fields(ctx, &entries)?;

    // Expose as global `Aike` — Aike-specific namespace
    ctx.global_obj()
        .define_property_value_str(ctx, "Aike", ns.into(), JS_PROP_ENUMERABLE)?;

    Ok(())
}
