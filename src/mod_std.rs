// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;

use zuqe::{
    CallContext, EvalFlags, EvalType, Exception, FieldDef, JS_SetUncatchableError, JSResult,
    JSValue, ScopedContext,
};

use crate::class_event_emitter::OS_PENDING_SIGNALS;

fn js_evalScript(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut backtrace_barrier = false;
    let mut is_async = false;

    if cc.length() >= 2 {
        let options_obj = cc.arg(1);

        is_async = options_obj.get_property_str(ctx, "async")?.to_bool()?;
        backtrace_barrier = options_obj
            .get_property_str(ctx, "backtrace_barrier")?
            .to_bool()?;
    }

    let str = cc.arg(0).to_string(ctx)?;

    thread_local! {
        static EVAL_SCRIPT_RECURSE: Cell<u32> = const { Cell::new(0) };
    }

    let lev = EVAL_SCRIPT_RECURSE.with(|s| {
        let n = s.get();
        s.set(n + 1);
        n + 1
    });

    if lev == 1 { /* interrupt handler is now installed in main() at startup */ }

    let mut flags = EvalFlags::empty();
    if backtrace_barrier {
        flags |= EvalFlags::BACKTRACE_BARRIER;
    }
    if is_async {
        flags |= EvalFlags::COMPILE_TO_ASYNC;
    }

    let ret = ctx.with_new_scope(|mut scope| {
        scope.eval(
            str.content().encode_utf8_bytes(true).as_ref(),
            "<evalScript>",
            EvalType::Global,
            flags,
        )
    });

    let lev = EVAL_SCRIPT_RECURSE.with(|s| {
        let n = s.get();
        s.set(n - 1);
        n - 1
    });
    if lev == 0 {
        OS_PENDING_SIGNALS.with(|s| {
            let n = s.get() & !(1_u64 << libc::SIGINT);
            s.set(n);
        });

        if ret.is_err() {
            // convert the uncatchable "interrupted" error into a normal error
            // so that it can be caught by the REPL
            if let Exception::Error(v) = ctx.rt().exception() {
                JS_SetUncatchableError(v, false);
            }
        }
    }

    ret
}

fn js_gc(ctx: &mut ScopedContext, _: CallContext) -> JSResult {
    ctx.rt_mut().gc_request();
    Ok(JSValue::UNDEFINED)
}

const MOD_STD_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("evalScript", 1, js_evalScript, 0),
    FieldDef::FUNC("gc", 0, js_gc, 0),
];

pub fn init_mod_std(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("std", MOD_STD_ENTRIES)?;
    Ok(())
}
