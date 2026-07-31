// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::RefCell, collections::HashMap, time::Instant};

use zuqe::{CallContext, EvalFlags, EvalType, FieldDef, JSResult, JSValue, Object, ScopedContext};

use crate::color::{Color, colorize};
use crate::inspect_value::js_inspect;

thread_local! {
    static GROUP_INDENT: RefCell<usize> = const { RefCell::new(0) };
    static COUNTERS: RefCell<HashMap<String, usize>> = RefCell::new(HashMap::new());
    static TIMERS: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
}

#[inline(always)]
fn console_println(msg: String) {
    crate::repl::print_output(msg.into());
}

/// Format a value for `console.log`/... output. Top-level strings are printed
/// raw (no quoting), matching Node's `console.log("hello") === hello`.
/// Other values fall back to inspect (which quotes nested strings).
fn console_format_arg(ctx: &mut ScopedContext, val: JSValue) -> String {
    if val.is_string() {
        val.as_string(false)
            .map(|s| s.encode_utf8(false).into_owned())
            .unwrap_or_default()
    } else {
        js_inspect(ctx, val)
    }
}

fn js_console_print(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let magic = cc.magic();
    let clr = || match magic {
        1 => Color::CyanFg,
        2 => Color::GreenFg,
        3 => Color::YellowFg,
        4 => Color::RedFg,
        _ => Color::Reset,
    };

    let mut out = String::new();
    let indent = GROUP_INDENT.with(|n| *n.borrow());
    for _ in 0..indent {
        out.push_str(&colorize("  ", clr()));
    }

    for i in 0..cc.length() {
        if i > 0 {
            out.push_str(&colorize(" ", clr()));
        }
        let arg = cc.arg(i);
        out.push_str(&colorize(console_format_arg(ctx, arg), clr()));
    }

    if magic == 4 {
        // error
        eprintln!("{}", out);
    } else {
        console_println(out);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_console_assert(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    if cc.length() == 0 || !cc.arg(0).to_bool()? {
        let mut out = String::new();
        let indent = GROUP_INDENT.with(|n| *n.borrow());
        for _ in 0..indent {
            out.push_str(&colorize("  ", Color::RedFg));
        }

        out.push_str(&colorize("Assertion failed", Color::RedFg));
        for i in 1..cc.length() {
            out.push_str(&colorize(" ", Color::RedFg));
            out.push_str(&colorize(js_inspect(ctx, cc.arg(i)), Color::RedFg));
        }

        eprintln!("{}", out);
    }

    Ok(JSValue::UNDEFINED)
}

fn js_console_trace(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let indent = GROUP_INDENT.with(|n| *n.borrow());
    let mut out = String::new();
    for _ in 0..indent {
        out.push_str(&colorize("  ", Color::Reset));
    }

    out.push_str(&colorize("Trace", Color::Reset));
    if cc.length() > 0 {
        out.push_str(&colorize(":", Color::Reset));
        for i in 0..cc.length() {
            out.push_str(&colorize(" ", Color::Reset));
            out.push_str(&colorize(js_inspect(ctx, cc.arg(i)), Color::Reset));
        }
    }
    console_println(out);

    // Append stack trace via JS eval
    ctx.with_new_scope(|scope| {
        if let Ok(stack_val) = scope.eval(
            b"new Error().stack",
            "<console.trace>",
            EvalType::Global,
            EvalFlags::empty(),
        ) && let Ok(s) = stack_val.to_string(scope)
        {
            let stack_str = format!("{}", s);
            for line in stack_str.lines().skip(1) {
                let mut out = String::new();
                for _ in 0..indent {
                    out.push_str(&colorize("  ", Color::Reset));
                }
                out.push_str(&colorize(line, Color::Reset));
                console_println(out);
            }
        }
    });

    Ok(JSValue::UNDEFINED)
}

fn js_console_dir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let indent = GROUP_INDENT.with(|n| *n.borrow());
    let mut out = String::new();
    for _ in 0..indent {
        out.push_str(&colorize("  ", Color::Reset));
    }
    if cc.length() > 0 {
        out.push_str(&js_inspect(ctx, cc.arg(0)));
    }
    console_println(out);
    Ok(JSValue::UNDEFINED)
}

fn js_console_count(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let label = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_string(ctx)?.to_string()
    } else {
        "default".to_string()
    };

    let count = COUNTERS.with(|c| {
        let mut map = c.borrow_mut();
        let n = map.entry(label.clone()).or_insert(0);
        *n += 1;
        *n
    });

    let mut out = String::new();
    let indent = GROUP_INDENT.with(|n| *n.borrow());
    for _ in 0..indent {
        out.push_str(&colorize("  ", Color::Reset));
    }
    out.push_str(&colorize(format!("{label}: {count}"), Color::Reset));
    console_println(out);

    Ok(JSValue::UNDEFINED)
}

fn js_console_count_reset(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let label = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_string(ctx)?.to_string()
    } else {
        "default".to_string()
    };

    COUNTERS.with(|c| {
        c.borrow_mut().remove(&label);
    });

    Ok(JSValue::UNDEFINED)
}

fn js_console_time(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let label = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_string(ctx)?.to_string()
    } else {
        "default".to_string()
    };

    TIMERS.with(|t| {
        let mut map = t.borrow_mut();
        if map.contains_key(&label) {
            let mut out = String::new();
            let indent = GROUP_INDENT.with(|n| *n.borrow());
            for _ in 0..indent {
                out.push_str(&colorize("  ", Color::YellowFg));
            }
            out.push_str(&colorize(
                format!("Timer '{label}' already exists"),
                Color::YellowFg,
            ));
            console_println(out);
        } else {
            map.insert(label, Instant::now());
        }
    });

    Ok(JSValue::UNDEFINED)
}

fn js_console_time_log(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let label = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_string(ctx)?.to_string()
    } else {
        "default".to_string()
    };

    let elapsed_ms = TIMERS.with(|t| {
        let map = t.borrow();
        map.get(&label)
            .map(|start| start.elapsed().as_secs_f64() * 1000.0)
    });

    let mut out = String::new();
    let indent = GROUP_INDENT.with(|n| *n.borrow());
    for _ in 0..indent {
        out.push_str(&colorize("  ", Color::Reset));
    }

    match elapsed_ms {
        Some(ms) => {
            out.push_str(&colorize(format!("{label}: {ms:.1} ms"), Color::Reset));
            for i in 1..cc.length() {
                out.push_str(&colorize(" ", Color::Reset));
                out.push_str(&colorize(js_inspect(ctx, cc.arg(i)), Color::Reset));
            }
            console_println(out);
        }
        None => {
            out.push_str(&colorize(
                format!("Timer '{label}' does not exist"),
                Color::YellowFg,
            ));
            console_println(out);
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_console_time_end(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let label = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_string(ctx)?.to_string()
    } else {
        "default".to_string()
    };

    let elapsed = TIMERS.with(|t| {
        let mut map = t.borrow_mut();
        map.remove(&label)
            .map(|start| start.elapsed().as_secs_f64() * 1000.0)
    });

    let mut out = String::new();
    let indent = GROUP_INDENT.with(|n| *n.borrow());
    for _ in 0..indent {
        out.push_str(&colorize("  ", Color::Reset));
    }

    match elapsed {
        Some(ms) => {
            out.push_str(&colorize(format!("{label}: {ms:.1} ms"), Color::Reset));
            console_println(out);
        }
        None => {
            out.push_str(&colorize(
                format!("Timer '{label}' does not exist"),
                Color::YellowFg,
            ));
            console_println(out);
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_console_group(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    GROUP_INDENT.with(|n| {
        *n.borrow_mut() += 1;
    });
    Ok(JSValue::UNDEFINED)
}

fn js_console_group_end(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    GROUP_INDENT.with(|n| {
        let mut indent = n.borrow_mut();
        if *indent > 0 {
            *indent -= 1;
        }
    });
    Ok(JSValue::UNDEFINED)
}

fn js_console_clear(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // ANSI clear screen + home cursor
    console_println(colorize("\x1b[2J\x1b[H", Color::Reset));
    Ok(JSValue::UNDEFINED)
}

fn js_console_table(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    if cc.length() == 0 {
        console_println(String::new());
        return Ok(JSValue::UNDEFINED);
    }

    let data = cc.arg(0);
    if data.is_undefined() {
        console_println("undefined".to_string());
        return Ok(JSValue::UNDEFINED);
    }
    if data.is_null() {
        console_println("null".to_string());
        return Ok(JSValue::UNDEFINED);
    }

    // Fallback: inspect-print the value
    console_println(js_inspect(ctx, data));
    Ok(JSValue::UNDEFINED)
}

/// Create and register the `console` object on globalThis.
pub(crate) fn install_console(ctx: &mut ScopedContext, global: &mut Object) -> Result<(), i32> {
    // `console` is a namespace object: its methods are own properties, and its
    // [[Prototype]] is an empty object whose own [[Prototype]] is %ObjectPrototype%.
    // WPT requires getPrototypeOf(console) to have no own properties.
    let proto = ctx.create_base_object()?;
    let mut console = ctx.create_base_object_with_proto(Some(proto))?;

    console.install_fields(
        ctx,
        &[
            FieldDef::FUNC("log", 0, js_console_print, 0),
            FieldDef::FUNC("debug", 0, js_console_print, 1),
            FieldDef::FUNC("info", 0, js_console_print, 2),
            FieldDef::FUNC("warn", 0, js_console_print, 3),
            FieldDef::FUNC("error", 0, js_console_print, 4),
            FieldDef::FUNC("assert", 0, js_console_assert, 0),
            FieldDef::FUNC("trace", 0, js_console_trace, 0),
            FieldDef::FUNC("dir", 0, js_console_dir, 0),
            FieldDef::FUNC("dirxml", 0, js_console_dir, 0),
            FieldDef::FUNC("count", 0, js_console_count, 0),
            FieldDef::FUNC("countReset", 0, js_console_count_reset, 0),
            FieldDef::FUNC("time", 0, js_console_time, 0),
            FieldDef::FUNC("timeLog", 0, js_console_time_log, 0),
            FieldDef::FUNC("timeEnd", 0, js_console_time_end, 0),
            FieldDef::FUNC("group", 0, js_console_group, 0),
            FieldDef::FUNC("groupCollapsed", 0, js_console_group, 0),
            FieldDef::FUNC("groupEnd", 0, js_console_group_end, 0),
            FieldDef::FUNC("clear", 0, js_console_clear, 0),
            FieldDef::FUNC("table", 0, js_console_table, 0),
            FieldDef::STR(
                "[Symbol.toStringTag]",
                "console",
                zuqe::JS_PROP_CONFIGURABLE,
            ),
        ],
    )?;

    global.define_property_value_str(
        ctx,
        "console",
        console.into(),
        zuqe::JS_PROP_CONFIGURABLE | zuqe::JS_PROP_WRITABLE,
    )?;

    Ok(())
}
