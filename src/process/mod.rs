// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod control;
pub(crate) mod info;
mod sys;

pub(crate) use sys::PROCESS_EXIT_CODE;

use zuqe::{Array, FieldDef, JS_PROP_ENUMERABLE, JSValue, ScopedContext};

use crate::{
    aike::JSStdioHandle,
    class_event_emitter::{EmitterPayload, JSEventEmitter},
};

/// Build and register the `process` global object (Node.js-compatible).
/// `process` is an EventEmitter instance, so `.on/.once/.emit/...` work on it.
pub fn init_process(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
    exec_path: &str,
) -> Result<(), i32> {
    let mut ns = JSEventEmitter::new_instance(ctx, JSValue::UNDEFINED, EmitterPayload::new())?;

    // execPath — the aike runtime executable path
    {
        let s = ctx.create_string_from_utf8(exec_path.as_bytes().to_vec())?;
        ns.define_property_value_str(ctx, "execPath", JSValue::make_string(s), JS_PROP_ENUMERABLE)?;
    }

    // argv / execArgv
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

    // pid / ppid — computed once at init
    ns.define_property_value_str(
        ctx,
        "pid",
        JSValue::from(std::process::id() as i32),
        JS_PROP_ENUMERABLE,
    )?;
    ns.define_property_value_str(
        ctx,
        "ppid",
        JSValue::from(info::get_ppid()),
        JS_PROP_ENUMERABLE,
    )?;

    // Process/thread-global mutators (exit, chdir, kill, umask, title setter)
    // are installed only on a root runtime; a child runtime never gets them.
    let is_root = ctx.rt().is_root();

    let mut entries: Vec<FieldDef> = vec![
        // --- proc ---
        FieldDef::FUNC("cwd", 0, sys::js_cwd, 0),
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
        // Node's `process.platform` uses "win32" (not Rust's "windows"), and
        // Node documents `os.platform()` as returning the same value — both are
        // kept in sync so `os.platform() === process.platform` holds.
        FieldDef::STR("platform", info::platform_str(), JS_PROP_ENUMERABLE),
        // --- info ---
        FieldDef::STR("arch", info::arch_str(), JS_PROP_ENUMERABLE),
        FieldDef::STR("version", info::VERSION_STR, JS_PROP_ENUMERABLE),
        FieldDef::LAZY_VALUE("versions", info::js_versions, JS_PROP_ENUMERABLE),
        FieldDef::GETSET(
            "title",
            Some(info::js_title_get),
            is_root.then_some(info::js_title_set),
            0,
        ),
        FieldDef::FUNC("uptime", 0, info::js_uptime, 0),
        // --- control ---
        FieldDef::FUNC("nextTick", 1, control::js_nextTick, 0),
        FieldDef::FUNC("memoryUsage", 0, control::js_memoryUsage, 0),
        FieldDef::FUNC("cpuUsage", 0, control::js_cpuUsage, 0),
        FieldDef::FUNC("hrtime", 1, control::js_hrtime, 0),
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

    if is_root {
        entries.push(FieldDef::FUNC("exit", 1, sys::js_exit, 0));
        entries.push(FieldDef::FUNC("chdir", 1, control::js_chdir, 0));
        entries.push(FieldDef::FUNC("kill", 2, control::js_kill, 0));
        entries.push(FieldDef::FUNC("umask", 1, control::js_umask, 0));
    }

    ns.install_fields(ctx, &entries)?;

    // Expose as global `process` — Node.js compatible
    ctx.global_obj()
        .define_property_value_str(ctx, "process", ns.into(), JS_PROP_ENUMERABLE)?;

    Ok(())
}
