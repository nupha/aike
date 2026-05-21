// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

pub(crate) mod signal;
mod spawn;
mod sys;

use zuqe::{Array, FieldDef, JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSValue, ScopedContext};

use crate::fs::class_file_handle::JSFileHandle;

/// Build and register the `Aike` global object.
pub fn init_aike(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
) -> Result<(), i32> {
    // Aike object inherits EventEmitter so process signals can be emitted on it
    // let mut ns =  class_event_emitter::create_event_emitter_instance(ctx)?;

    let mut ns = ctx.create_base_object()?;

    // argv / execArgv require runtime data — build JS arrays dynamically
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(argv.len());
        for a in argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(s.into());
        }
        let arr = Array::from_values(ctx, elems)?;
        ns.define_property_value_str(ctx, "argv", arr.into(), JS_PROP_C_W_E)?;
    }
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(exec_argv.len());
        for a in exec_argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(s.into());
        }
        let arr = Array::from_values(ctx, elems)?;
        ns.define_property_value_str(ctx, "execArgv", arr.into(), JS_PROP_C_W_E)?;
    }

    let entries: &[FieldDef] = &[
        // --- proc ---
        FieldDef::FUNC("cwd", 0, sys::js_cwd, 0),
        FieldDef::FUNC("exit", 1, sys::js_exit, 0),
        FieldDef::FUNC("env", 0, sys::js_env, 0),
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
        // --- signal ---
        FieldDef::FUNC("onSignal", 2, signal::js_on_signal, 0),
    ];

    ns.install_fields(ctx, entries)?;

    // Expose as global `Aike`
    ctx.global_obj()
        .define_property_value_str(ctx, "Aike", ns.into(), JS_PROP_ENUMERABLE)?;
    Ok(())
}
