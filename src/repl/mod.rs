// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

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
static STDIO_CAPTURED: AtomicBool = AtomicBool::new(false);

/// Toggle stdio mode: when `b` is true, all subsequent print_output() calls
/// use stderr (keeping stdout clean for the MCP stdio transport); when false,
/// stdio capture is released.
///
/// Passing `true` while already captured returns an error ("stdio already
/// captured") so a second server cannot clobber an active stdio transport.
pub(crate) fn caupture_stdio(b: bool) -> Result<(), String> {
    if b && STDIO_CAPTURED.load(Ordering::Acquire) {
        return Err("stdio already captured".into());
    }
    STDIO_CAPTURED.store(b, Ordering::Release);
    Ok(())
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
    if STDIO_CAPTURED.load(Ordering::Acquire) {
        eprintln!("{}", msg);
    } else if IN_READLINE.load(Ordering::Acquire)
        && let Some(mut p) = PRINTER.get().and_then(|x| x.lock().ok())
    {
        let _ = p.print(msg.to_string());
    } else {
        println!("{}", msg);
    }
}
