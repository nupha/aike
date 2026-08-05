// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_io_handle;
mod cron;
mod signal;
mod spawn;
mod sys;

pub(crate) use {
    class_io_handle::{JSFileHandle, JSStdioHandle, gc_trace_io, make_stats},
    cron::{JSCronJob, gc_trace_cron},
    signal::{gc_trace_signals, register_signal_handler, signal_number},
    spawn::terminate_process,
    sys::get_time_ns,
};

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, ScopedContext};

/// Build and register the `Aike` global object (Aike-specific APIs).
pub fn init_aike(ctx: &mut ScopedContext) -> Result<(), i32> {
    let mut ns = ctx.create_base_object()?;

    let entries: &[FieldDef] = &[
        // --- spawn ---
        FieldDef::FUNC("spawnSync", 0, spawn::js_spawn, 0),
        FieldDef::FUNC("spawn", 0, spawn::js_spawn, 1),
        // --- sys ---
        FieldDef::FUNC("evalScript", 1, sys::js_evalScript, 0),
        FieldDef::FUNC("evalModule", 2, sys::js_evalModule, 0),
        FieldDef::FUNC("gc", 0, sys::js_gc, 0),
        FieldDef::FUNC("isatty", 1, sys::js_isatty, 0),
        FieldDef::FUNC("ttyGetWinSize", 1, sys::js_ttyGetWinSize, 0),
        FieldDef::FUNC("ttySetRaw", 1, sys::js_ttySetRaw, 0),
        FieldDef::FUNC("now", 0, sys::js_now, 0),
        FieldDef::FUNC("homedir", 0, sys::js_homedir, 0),
        FieldDef::FUNC("tmpdir", 0, sys::js_tmpdir, 0),
        FieldDef::FUNC("which", 1, sys::js_which, 0),
        // --- gzip (non-WHATWG; Aike convenience helpers) ---
        FieldDef::FUNC("gzip", 1, crate::gzip::js_gzip, 0),
        FieldDef::FUNC("gunzip", 1, crate::gzip::js_gunzip, 0),
        // --- signal ---
        FieldDef::FUNC("onSignal", 2, signal::js_on_signal, 0),
        FieldDef::LAZY_VALUE("signals", signal::js_signals, JS_PROP_ENUMERABLE),
        // --- cron / timer ---
        FieldDef::FUNC("schedule", 2, cron::js_schedule, 0),
        FieldDef::FUNC("sleep", 0, crate::whatwg::timeout::js_sleep, 0),
    ];

    ns.install_fields(ctx, entries)?;

    // Expose as global `Aike` — Aike-specific namespace
    ctx.global_obj()
        .define_property_value_str(ctx, "Aike", ns.into(), JS_PROP_ENUMERABLE)?;

    Ok(())
}
