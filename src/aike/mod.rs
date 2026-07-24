// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_io_handle;
pub(crate) mod cron;
pub(crate) mod signal;
mod spawn;
mod sys;

#[allow(unused_imports)]
pub(crate) use signal::{register_signal_handler, unregister_signal_handler};
pub(crate) use {
    class_io_handle::{JSFileHandle, JSStdioHandle, gc_trace_io, make_stats},
    cron::{JSCronJob, gc_trace_cron},
};

use zuqe::{Array, FieldDef, JS_PROP_ENUMERABLE, JSValue, ScopedContext};

/// Build and register the `Aike` global object.
pub fn init_aike(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
    exec_path: &str,
) -> Result<(), i32> {
    // Aike object inherits EventEmitter so process signals can be emitted on it
    // let mut ns =  class_event_emitter::create_event_emitter_instance(ctx)?;

    let mut ns = ctx.create_base_object()?;

    // execPath — the aike runtime executable path
    {
        let s = ctx.create_string_from_utf8(exec_path.as_bytes().to_vec())?;
        ns.define_property_value_str(ctx, "execPath", JSValue::make_string(s), JS_PROP_ENUMERABLE)?;
    }

    // argv / execArgv require runtime data — build JS arrays dynamically
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(argv.len());
        for a in argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(JSValue::make_string(s));
        }
        let arr = Array::from_values(ctx, elems)?;
        ns.define_property_value_str(ctx, "argv", arr.into(), JS_PROP_ENUMERABLE)?;
    }
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(exec_argv.len());
        for a in exec_argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(JSValue::make_string(s));
        }
        let arr = Array::from_values(ctx, elems)?;
        ns.define_property_value_str(ctx, "execArgv", arr.into(), JS_PROP_ENUMERABLE)?;
    }

    let entries: &[FieldDef] = &[
        // --- proc ---
        FieldDef::FUNC("cwd", 0, sys::js_cwd, 0),
        FieldDef::FUNC("exit", 1, sys::js_exit, 0),
        FieldDef::LAZY_VALUE(
            "env",
            |cx| {
                let mut env_obj = cx.create_base_object()?;
                for (key, value) in std::env::vars() {
                    let v = cx.create_string_from_utf8(value.as_bytes().to_vec())?;
                    env_obj.set_property_str(cx, &key, JSValue::make_string(v))?;
                }
                Ok(env_obj.into())
            },
            JS_PROP_ENUMERABLE,
        ),
        // --- spawn ---
        FieldDef::FUNC("spawnSync", 0, spawn::js_spawn, 0),
        FieldDef::FUNC("spawn", 0, spawn::js_spawn, 1),
        // --- sys ---
        FieldDef::STR(
            "platform",
            {
                match std::env::consts::OS {
                    "windows" => "windows",
                    "macos" => "darwin",
                    other => other,
                }
            },
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::FUNC("evalScript", 1, sys::js_evalScript, 0),
        FieldDef::FUNC("gc", 0, sys::js_gc, 0),
        FieldDef::FUNC("isatty", 1, sys::js_isatty, 0),
        FieldDef::FUNC("ttyGetWinSize", 1, sys::js_ttyGetWinSize, 0),
        FieldDef::FUNC("ttySetRaw", 1, sys::js_ttySetRaw, 0),
        FieldDef::FUNC("now", 0, sys::js_now, 0),
        FieldDef::FUNC("homedir", 0, sys::js_homedir, 0),
        // --- signal ---
        FieldDef::FUNC("onSignal", 2, signal::js_on_signal, 0),
        FieldDef::LAZY_VALUE("signals", signal::js_signals, JS_PROP_ENUMERABLE),
        // --- cron / timer ---
        FieldDef::FUNC("schedule", 2, cron::js_schedule, 0),
        FieldDef::FUNC("sleep", 0, crate::whatwg::timeout::js_sleep, 0),
        // --- stdio ---
        FieldDef::LAZY_VALUE(
            "stdin",
            |cx| JSStdioHandle::new_instance_stdin(cx).map(JSValue::from),
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::LAZY_VALUE(
            "stdout",
            |cx| JSStdioHandle::new_instance_stdout(cx).map(JSValue::from),
            JS_PROP_ENUMERABLE,
        ),
        FieldDef::LAZY_VALUE(
            "stderr",
            |cx| JSStdioHandle::new_instance_stderr(cx).map(JSValue::from),
            JS_PROP_ENUMERABLE,
        ),
    ];

    ns.install_fields(ctx, entries)?;

    // Expose as global `Aike`
    ctx.global_obj()
        .define_property_value_str(ctx, "Aike", ns.into(), JS_PROP_ENUMERABLE)?;
    Ok(())
}
