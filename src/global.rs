// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use zuqe::ScopedContext;

use crate::{aike::init_aike, whatwg::install_whatwg};

pub fn setup_globals(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
    exec_path: &str,
) -> Result<(), i32> {
    let global = ctx.global_obj();

    // Install all WHATWG-standard global APIs (console, timers, etc.)
    install_whatwg(ctx, global)?;

    // Aike — branding namespace
    init_aike(ctx, argv, exec_argv, exec_path)?;

    Ok(())
}
