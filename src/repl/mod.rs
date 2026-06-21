// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod completion;
mod prompt;
mod worker;

pub(crate) use worker::run;

use std::{
    borrow::Cow,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use rustyline::ExternalPrinter;

/// Set to true while readline() is active (raw mode), so output uses
/// ExternalPrinter instead of direct stdout.
pub(super) static IN_READLINE: AtomicBool = AtomicBool::new(false);

/// When true, print_output() routes all console.log/warn/error to stderr,
/// keeping stdout clean for the MCP stdio transport.
static STDIO_MODE: AtomicBool = AtomicBool::new(false);

/// Enable stdio mode: all subsequent print_output() calls use stderr.
/// Called by McpStdio::start() before the server begins.
pub(crate) fn set_stdio_mode() {
    STDIO_MODE.store(true, Ordering::Release);
}

/// Global printer used by print_output(). Initialized once at REPL startup.
static PRINTER: OnceLock<Mutex<Box<dyn ExternalPrinter + Send>>> = OnceLock::new();

/// Initialize the global printer (called once at REPL startup).
pub(super) fn init_printer(printer: Box<dyn ExternalPrinter + Send>) {
    let _ = PRINTER.set(Mutex::new(printer));
}

/// Print a message to the terminal. Uses ExternalPrinter during readline()
/// (raw mode), stderr in stdio mode, and direct stdout otherwise.
pub(crate) fn print_output<'a>(msg: Cow<'a, str>) {
    if STDIO_MODE.load(Ordering::Acquire) {
        eprintln!("{}", msg);
    } else if IN_READLINE.load(Ordering::Acquire)
        && let Some(mut p) = PRINTER.get().and_then(|x| x.lock().ok())
    {
        let _ = p.print(msg.to_string());
    } else {
        println!("{}", msg);
    }
}
