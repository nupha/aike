// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use zuqe::ScopedContext;

use crate::{
    aike::{install_aike_namespace, save_process_info},
    process::init_process,
    whatwg::install_whatwg,
};

pub fn setup_globals(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
    exec_path: &str,
) -> Result<(), i32> {
    let global = ctx.global_obj();

    // Stash argv/exec info so a child runtime granted the `process` capability
    // can build an equivalent namespace.
    save_process_info(argv, exec_argv, exec_path);

    // Install all WHATWG-standard global APIs (console, timers, etc.)
    install_whatwg(ctx, global)?;

    // process — Node.js-compatible namespace
    init_process(ctx, argv, exec_argv, exec_path)?;

    // Aike — Aike-specific namespace
    install_aike_namespace(ctx)?;

    Ok(())
}
