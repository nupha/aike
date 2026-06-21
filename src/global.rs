// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use zuqe::{Callback, ScopedContext};

use crate::{aike::init_aike, request::js_request, whatwg::install_whatwg};

pub fn setup_globals(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
    exec_path: &str,
) -> Result<(), i32> {
    let mut global = ctx.global_obj();

    // Install all WHATWG-standard global APIs (console, timers, etc.)
    install_whatwg(ctx, global)?;

    // Aike — branding namespace
    init_aike(ctx, argv, exec_argv, exec_path)?;

    // request
    {
        let func = ctx.create_native_function(Callback::Func(js_request), "request", 2, 0)?;
        global.define_property_value_str(ctx, "request", func.into(), zuqe::JS_PROP_C_W_E)?;
    }

    Ok(())
}
