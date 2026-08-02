// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, process::ExitCode};

use zuqe::{
    Array, CallContext, EvalFlags, EvalType, JS_PROP_C_W_E, JSResult, JSValue, ScopedContext,
};

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

// Declare windows crt functions
#[cfg(windows)]
unsafe extern "system" {
    fn _get_osfhandle(fd: i32) -> windows_sys::Win32::Foundation::HANDLE;
    fn _setmode(fd: i32, mode: i32) -> i32;
}

// ucrt invalid-parameter handling. Passing an out-of-range fd to `_isatty`
// (or other lowio functions) makes ucrt invoke the invalid-parameter handler,
// which by default calls `__fastfail` and terminates the process with
// STATUS_STACK_BUFFER_OVERRUN (0xC0000409). We install a per-thread no-op
// handler around such calls so they fail gracefully instead of aborting.
#[cfg(windows)]
type CrtInvalidParamHandler =
    Option<unsafe extern "C" fn(*const u16, *const u16, *const u16, u32, usize)>;

#[cfg(windows)]
unsafe extern "C" {
    fn _set_thread_local_invalid_parameter_handler(
        handler: CrtInvalidParamHandler,
    ) -> CrtInvalidParamHandler;
}

#[cfg(windows)]
unsafe extern "C" fn silent_invalid_parameter_handler(
    _expression: *const u16,
    _function: *const u16,
    _file: *const u16,
    _line: u32,
    _reserved: usize,
) {
    // Swallow the invalid parameter instead of letting ucrt __fastfail.
}

/// eval script string in global mode
pub(super) fn js_evalScript(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut stack_barrier = false;
    let mut top_level_await = false;
    let mut base_url = "<evalScript>".to_string();

    if cc.length() >= 2 {
        let options_obj = cc.arg(1);

        top_level_await = options_obj
            .get_property_str(ctx, "topLevelAwait")?
            .to_bool()?;
        stack_barrier = options_obj
            .get_property_str(ctx, "stackBarrier")?
            .to_bool()?;
        let fn_val = options_obj.get_property_str(ctx, "baseUrl")?;
        if !fn_val.is_undefined() {
            base_url =
                String::from_utf8_lossy(&fn_val.to_string(ctx)?.content().encode_utf8_bytes(true))
                    .into_owned();
        }
    }

    let str = cc.arg(0).to_string(ctx)?;

    thread_local! {
        static EVAL_SCRIPT_RECURSE: Cell<u32> = const { Cell::new(0) };
    }

    let lev = EVAL_SCRIPT_RECURSE.with(|s| {
        let n = s.get();
        s.set(n + 1);
        n + 1
    });

    if lev == 1 { /* interrupt handler is now installed in main() at startup */ }

    let mut flags = EvalFlags::empty();
    if stack_barrier {
        flags |= EvalFlags::BACKTRACE_BARRIER;
    }
    if top_level_await {
        flags |= EvalFlags::COMPILE_TO_ASYNC;
    }

    ctx.with_new_scope(|cx| {
        cx.eval(
            str.content().encode_utf8_bytes(true).as_ref(),
            &base_url,
            EvalType::Global,
            flags,
        )
    })
}

/// Evaluate a string as an ES module with an explicit base URL.
///
/// Unlike `evalScript` (which runs in script/global mode), this compiles the
/// code as a module, so `import`/`export` and top-level `await` are allowed and
/// `import.meta.url` equals `base_url`. Relative `fs`/`fetch` specifiers inside
/// the module also resolve against `base_url`.
///
/// `base_url` must be a valid `file://`, `http://` or `https://` URL and acts
/// as the module's identity and the base for relative resolution.
pub(super) fn js_evalModule(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let str = cc.arg(0).to_string(ctx)?;
    let raw_url = cc.arg(1).to_string(ctx)?;
    let base_url = raw_url.content().encode_utf8_bytes(true);
    let base_url = String::from_utf8_lossy(&base_url).into_owned();

    // Validate scheme so relative resolution has a well-defined base.
    match url::Url::parse(&base_url) {
        Ok(u) if matches!(u.scheme(), "file" | "http" | "https") => {}
        Ok(u) => {
            return Err(ctx.throw_type_error(&format!(
                "evalModule: unsupported base url scheme '{}' (expected file/http/https)",
                u.scheme()
            )));
        }
        Err(e) => {
            return Err(
                ctx.throw_type_error(&format!("evalModule: invalid base url '{base_url}': {e}"))
            );
        }
    }

    // `base_url` becomes the module name, which backs `import.meta.url` and is
    // reachable via `ScopedContext::current_module_name` for relative `fs`/`fetch`
    // resolution (no thread-local stack needed; the engine frame carries it).
    ctx.with_new_scope(|cx| {
        cx.eval(
            str.content().encode_utf8_bytes(true).as_ref(),
            &base_url,
            EvalType::Module,
            EvalFlags::empty(),
        )
    })
}

pub(super) fn js_gc(ctx: &mut ScopedContext, _: CallContext) -> JSResult {
    ctx.rt_mut().gc_request();
    Ok(JSValue::UNDEFINED)
}

pub(super) fn js_now(_: &mut ScopedContext, _: CallContext) -> JSResult {
    Ok(JSValue::from(get_time_ns() as f64 / 1e6))
}

#[inline]
fn get_time_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }

    #[cfg(windows)]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |t| t.as_nanos() as u64)
    }
}

pub(super) fn js_isatty(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let fd = cc.arg(0).to_number(ctx)?.to_i32();

    // Negative fds are never a tty.
    if fd < 0 {
        return Ok(JSValue::FALSE);
    }

    #[cfg(windows)]
    let is_tty = unsafe {
        // Guard against ucrt __fastfail when `fd` is out of range.
        let prev =
            _set_thread_local_invalid_parameter_handler(Some(silent_invalid_parameter_handler));
        let r = libc::isatty(fd);
        _set_thread_local_invalid_parameter_handler(prev);
        r != 0
    };

    #[cfg(not(windows))]
    let is_tty = unsafe { libc::isatty(fd) != 0 };

    Ok(JSValue::from(is_tty))
}

pub(super) fn js_ttyGetWinSize(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let fd = cc.arg(0).to_number(ctx)?.to_i32();
    let w: i32;
    let h: i32;

    #[cfg(unix)]
    {
        let mut sz = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut sz) == 0 }
            && sz.ws_col >= 4
            && sz.ws_row >= 4
        {
            w = sz.ws_col as i32;
            h = sz.ws_row as i32;
        } else {
            return Ok(JSValue::NULL);
        }
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo,
        };

        let mut info = std::mem::zeroed::<CONSOLE_SCREEN_BUFFER_INFO>();
        if GetConsoleScreenBufferInfo(_get_osfhandle(fd), &mut info) != 0 {
            w = info.dwSize.X as i32;
            h = info.dwSize.Y as i32;
        } else {
            return Ok(JSValue::NULL);
        }
    }

    let mut obj = Array::create(ctx)?;
    obj.define_property_value_u32(ctx, 0, JSValue::from(w), JS_PROP_C_W_E)?;
    obj.define_property_value_u32(ctx, 1, JSValue::from(h), JS_PROP_C_W_E)?;
    Ok(obj.into())
}

pub(super) fn js_ttySetRaw(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let fd = cc.arg(0).to_number(ctx)?.to_i32();

    #[cfg(unix)]
    unsafe {
        let mut tty: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tty) != 0 {
            return Err(
                ctx.throw_type_error(&format!("cannot set raw mode on fd {}: not a terminal", fd))
            );
        }

        // Save original terminal state (once, first successful call only).
        static mut OLD_TTY_SAVED: bool = false;
        static mut OLD_TTY: libc::termios = unsafe { std::mem::zeroed::<libc::termios>() };
        if !OLD_TTY_SAVED {
            OLD_TTY = tty;
            OLD_TTY_SAVED = true;

            extern "C" fn term_exit() {
                unsafe {
                    libc::tcsetattr(0, libc::TCSANOW, &raw const OLD_TTY);
                }
            }
            libc::atexit(term_exit);
        }

        tty.c_iflag &= !(libc::IGNBRK
            | libc::BRKINT
            | libc::PARMRK
            | libc::ISTRIP
            | libc::INLCR
            | libc::IGNCR
            | libc::ICRNL
            | libc::IXON);
        tty.c_oflag |= libc::OPOST;
        tty.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::IEXTEN);
        tty.c_cflag &= !(libc::CSIZE | libc::PARENB);
        tty.c_cflag |= libc::CS8;
        tty.c_cc[libc::VMIN] = 1;
        tty.c_cc[libc::VTIME] = 0;

        libc::tcsetattr(fd, libc::TCSANOW, &tty);
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{
            ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, ENABLE_WRAP_AT_EOL_OUTPUT,
            SetConsoleMode,
        };

        SetConsoleMode(
            _get_osfhandle(fd),
            ENABLE_WINDOW_INPUT | ENABLE_VIRTUAL_TERMINAL_INPUT,
        );
        _setmode(fd, libc::O_BINARY);

        if fd == 0 {
            SetConsoleMode(
                _get_osfhandle(1),
                ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
            );
        }
    }

    Ok(JSValue::UNDEFINED)
}

pub(super) fn js_homedir(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    match dirs::home_dir() {
        Some(p) => ctx
            .create_string_from_utf8(p.to_string_lossy().as_bytes().to_vec())
            .map(JSValue::make_string),
        None => Err(ctx.throw_internal_error("cannot determine home directory")),
    }
}
