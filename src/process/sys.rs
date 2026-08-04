// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::process::ExitCode;

use zuqe::{CallContext, JSResult, JSValue, ScopedContext};

pub(super) fn js_cwd(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    match std::env::current_dir() {
        Ok(p) => ctx
            .create_string_from_utf8(p.to_string_lossy().as_bytes().to_vec())
            .map(JSValue::make_string),
        Err(e) => Err(ctx.throw_internal_error(&e.to_string())),
    }
}

pub(super) fn js_exit(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let code = if !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(ctx).map(|n| n.to_i32()).unwrap_or(0)
    } else {
        0
    };

    // Set the runtime interrupt flag rather than calling `std::process::exit`:
    // the latter skips destructors and leaks GC nodes. The interrupt makes
    // `run_async` return `Interrupted` on the next tick, so `main.rs` exits
    // through the normal path and finalizes GC nodes.
    ctx.rt_mut().interrupt(ExitCode::from(code as u8));

    // Return Ok instead of Err: interrupt flag is set on runtime, tick() detects
    // it on the next synchronization point.  Returning Err would cause async
    // callers to reject a Promise, triggering user-level .catch() handlers.
    Ok(JSValue::UNDEFINED)
}
