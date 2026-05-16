// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ops::Deref;

use colorized::{Colors, colorize_print, colorize_println};
use zuqe::{
    CallContext, Callback, JS_PROP_C_W_E, JS_PROP_WRITABLE, JSResult, JSValue, ScopedContext,
};

use crate::{process::build_process_object, request::js_request};

fn js_print(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let level = cc.magic() % 100;
    let inspect = true;

    let color = || {
        match level {
            1 => Colors::CyanFg,   // debug
            2 => Colors::GreenFg,  // info
            3 => Colors::YellowFg, // warn
            4 => Colors::RedFg,    // error
            _ => Colors::WhiteFg,
        }
    };

    for i in 0..cc.length() {
        if i > 0 {
            colorize_print(" ", color());
        }

        let arg = cc.arg(i);

        colorize_print(
            if inspect {
                arg.inspect(ctx.rt_mut())?.to_string()
            } else {
                format!("{}", arg.to_string(ctx)?.deref())
            },
            color(),
        );
    }

    if cc.magic() >= 100 {
        // print no newline
        colorize_print("", color());
    } else {
        colorize_println("", color());
    }

    Ok(JSValue::UNDEFINED)
}

pub fn setup_globals(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
) -> Result<(), i32> {
    let mut global = ctx.global_obj();

    {
        let print_no_nl = ctx.create_native_function(Callback::Func(js_print), "print", 0, 100)?;
        global.set_property_str(ctx, "print", print_no_nl.into())?;
        let debug_no_nl = ctx.create_native_function(Callback::Func(js_print), "debug", 0, 101)?;
        global.set_property_str(ctx, "debug", debug_no_nl.into())?;

        let mut console = ctx.create_base_object()?;

        let log_func = ctx.create_native_function(Callback::Func(js_print), "print", 0, 0)?;
        console.define_property_value_str(ctx, "log", log_func.into(), JS_PROP_C_W_E)?;

        let debug_func = ctx.create_native_function(Callback::Func(js_print), "debug", 0, 1)?;
        console.define_property_value_str(ctx, "debug", debug_func.into(), JS_PROP_C_W_E)?;

        let info_func = ctx.create_native_function(Callback::Func(js_print), "info", 0, 2)?;
        console.define_property_value_str(ctx, "info", info_func.into(), JS_PROP_C_W_E)?;

        let warn_func = ctx.create_native_function(Callback::Func(js_print), "warn", 0, 3)?;
        console.define_property_value_str(ctx, "warn", warn_func.into(), JS_PROP_C_W_E)?;

        let error_func = ctx.create_native_function(Callback::Func(js_print), "error", 0, 4)?;
        console.define_property_value_str(ctx, "error", error_func.into(), JS_PROP_C_W_E)?;

        global.define_property_value_str(ctx, "console", console.into(), JS_PROP_C_W_E)?;
    }

    // process
    {
        let process = build_process_object(ctx, argv, exec_argv)?;
        global.define_property_value_str(ctx, "process", process.into(), JS_PROP_WRITABLE)?;
    }

    // request
    {
        let func = ctx.create_native_function(Callback::Func(js_request), "request", 2, 0)?;
        global.define_property_value_str(ctx, "request", func.into(), zuqe::JS_PROP_C_W_E)?;
    }

    Ok(())
}
