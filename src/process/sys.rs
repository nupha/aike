// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    process::ExitCode,
    sync::atomic::{AtomicI32, Ordering},
};

use zuqe::{CallContext, JSResult, JSValue, ScopedContext};

/// Raw exit code set by `process.exit`, read by main to exit the process
/// directly (bypassing tokio runtime shutdown that would otherwise wait on
/// orphaned background tasks). ExitCode is opaque and has no stable extraction,
/// so we capture the i32 code here.
pub(crate) static PROCESS_EXIT_CODE: AtomicI32 = AtomicI32::new(0);

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

    // Capture the raw code so main can `std::process::exit` with it (ExitCode is
    // opaque). Then set the runtime interrupt flag rather than calling
    // `std::process::exit` directly: the latter skips destructors and leaks GC
    // nodes. The interrupt makes `run_async` return `Interrupted` on the next
    // tick, so `main.rs` finalizes GC nodes (drop(rt)) and then exits.
    PROCESS_EXIT_CODE.store(code, Ordering::SeqCst);
    ctx.rt_mut().interrupt(ExitCode::from(code as u8));

    // Return Ok instead of Err: interrupt flag is set on runtime, tick() detects
    // it on the next synchronization point.  Returning Err would cause async
    // callers to reject a Promise, triggering user-level .catch() handlers.
    Ok(JSValue::UNDEFINED)
}
