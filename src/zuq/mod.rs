// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! zuq/mod.rs — unified entry point for the `zuq` global object.
//!
//! The `zuq` object inherits from EventEmitter so JS code can call
//! `zuq.on('SIGINT', handler)` to receive process signals.

mod proc;
mod spawn;
mod sys;

use zuqe::{
    Array, FieldDef, JS_PROP_C_W_E, JS_PROP_CONFIGURABLE, JS_PROP_ENUMERABLE, JS_PROP_WRITABLE,
    JSValue, ScopedContext,
};

use crate::{
    class_event_emitter::create_event_emitter_instance, fs::class_file_handle::JSFileHandle,
};

/// Build and register the `zuq` global object.
///
/// argv and execArgv are dynamic so they are defined first.
/// All other fields are declared in one unified `entries` slice
/// and installed with a single `install_fields` call.
pub fn init_zuq(ctx: &mut ScopedContext, argv: &[String], exec_argv: &[String]) -> Result<(), i32> {
    // zuq inherits EventEmitter so process signals can be emitted on it
    let mut zuq = create_event_emitter_instance(ctx)?;

    // argv / execArgv require runtime data — build JS arrays dynamically
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(argv.len());
        for a in argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(s.into());
        }
        let arr = Array::from_values(ctx, elems)?;
        zuq.define_property_value_str(ctx, "argv", arr.into(), JS_PROP_C_W_E)?;
    }
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(exec_argv.len());
        for a in exec_argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(s.into());
        }
        let arr = Array::from_values(ctx, elems)?;
        zuq.define_property_value_str(ctx, "execArgv", arr.into(), JS_PROP_C_W_E)?;
    }

    // All remaining fields combined into one slice:
    //   proc  — cwd, exit, env
    //   stdio — stdin, stdout, stderr  (inlined; stdio.rs had no other content)
    //   spawn — spawnSync, spawn
    //   sys   — evalScript, gc, now, isatty, tty*, signal consts
    let entries: &[FieldDef] = &[
        // --- proc ---
        FieldDef::FUNC("cwd", 0, proc::js_cwd, 0),
        FieldDef::FUNC("exit", 1, proc::js_exit, 0),
        FieldDef::FUNC("env", 0, proc::js_env, 0),
        // --- stdio ---
        FieldDef::VALUE(
            "stdin",
            |cx| JSFileHandle::new_instance_stdin(cx).map(JSValue::Object),
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::VALUE(
            "stdout",
            |cx| JSFileHandle::new_instance_stdout(cx).map(JSValue::Object),
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::VALUE(
            "stderr",
            |cx| JSFileHandle::new_instance_stderr(cx).map(JSValue::Object),
            JS_PROP_ENUMERABLE,
        ),
        // --- spawn ---
        FieldDef::FUNC("spawnSync", 0, spawn::js_spawn, 0),
        FieldDef::FUNC("spawn", 0, spawn::js_spawn, 1),
        // --- sys ---
        FieldDef::FUNC("evalScript", 1, sys::js_evalScript, 0),
        FieldDef::FUNC("gc", 0, sys::js_gc, 0),
        FieldDef::FUNC("isatty", 1, sys::js_isatty, 0),
        FieldDef::FUNC("ttyGetWinSize", 1, sys::js_ttyGetWinSize, 0),
        FieldDef::FUNC("ttySetRaw", 1, sys::js_ttySetRaw, 0),
        FieldDef::FUNC("now", 0, sys::js_now, 0),
        // signals
        // FieldDef::INT32("SIGINT", libc::SIGINT, JS_PROP_CONFIGURABLE),
        // FieldDef::INT32("SIGABRT", libc::SIGABRT, JS_PROP_CONFIGURABLE),
        // FieldDef::INT32("SIGFPE", libc::SIGFPE, JS_PROP_CONFIGURABLE),
        // FieldDef::INT32("SIGILL", libc::SIGILL, JS_PROP_CONFIGURABLE),
        // FieldDef::INT32("SIGSEGV", libc::SIGSEGV, JS_PROP_CONFIGURABLE),
        // FieldDef::INT32("SIGTERM", libc::SIGTERM, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGQUIT", libc::SIGQUIT, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGPIPE", libc::SIGPIPE, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGALRM", libc::SIGALRM, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGUSR1", libc::SIGUSR1, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGUSR2", libc::SIGUSR2, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGCHLD", libc::SIGCHLD, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGCONT", libc::SIGCONT, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGSTOP", libc::SIGSTOP, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGTSTP", libc::SIGTSTP, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGTTIN", libc::SIGTTIN, JS_PROP_CONFIGURABLE),
        // #[cfg(not(windows))]
        // FieldDef::INT32("SIGTTOU", libc::SIGTTOU, JS_PROP_CONFIGURABLE),
    ];

    zuq.install_fields(ctx, entries)?;

    // Expose as writable global `zuq`
    let mut global = ctx.global_obj();
    global.define_property_value_str(ctx, "zuq", zuq.into(), JS_PROP_WRITABLE)?;
    Ok(())
}
