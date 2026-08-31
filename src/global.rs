// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use zuqe::ScopedContext;

use crate::{aike::init_aike, process::init_process, whatwg::install_whatwg};

pub fn setup_globals(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
    exec_path: &str,
    #[cfg(feature = "local_storage")] local_storage_db: Option<&str>,
) -> Result<(), i32> {
    let global = ctx.global_obj();

    // Install all WHATWG-standard global APIs (console, timers, etc.)
    install_whatwg(
        ctx,
        global,
        #[cfg(feature = "local_storage")]
        local_storage_db,
    )?;

    // process — Node.js-compatible namespace
    init_process(ctx, argv, exec_argv, exec_path)?;

    // Aike — Aike-specific namespace
    init_aike(ctx)?;

    Ok(())
}
