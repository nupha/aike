// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;

use zuqe::{
    Array, CallContext, EvalFlags, EvalType, JS_PROP_C_W_E, JSResult, JSValue, ScopedContext,
};

pub(super) fn js_cwd(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    match std::env::current_dir() {
        Ok(p) => ctx
            .create_string_from_utf8(p.to_string_lossy().as_bytes().to_vec())
            .map(JSValue::from),
        Err(e) => {
            ctx.throw_internal_error(&e.to_string());
            Err(-1)
        }
    }
}

pub(super) fn js_exit(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    use std::process::ExitCode;
    let code = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(ctx).map(|n| n.to_i32()).unwrap_or(0)
    } else {
        0
    };
    ctx.rt_mut().interrupt(ExitCode::from(code as u8));
    Err(-1)
}

// Declare windows crt functions
#[cfg(windows)]
unsafe extern "system" {
    fn _get_osfhandle(fd: i32) -> windows_sys::Win32::Foundation::HANDLE;
    fn _setmode(fd: i32, mode: i32) -> i32;
}

pub(super) fn js_evalScript(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut backtrace_barrier = false;
    let mut is_async = false;

    if cc.length() >= 2 {
        let options_obj = cc.arg(1);

        is_async = options_obj.get_property_str(ctx, "async")?.to_bool()?;
        backtrace_barrier = options_obj
            .get_property_str(ctx, "backtrace_barrier")?
            .to_bool()?;
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
    if backtrace_barrier {
        flags |= EvalFlags::BACKTRACE_BARRIER;
    }
    if is_async {
        flags |= EvalFlags::COMPILE_TO_ASYNC;
    }

    ctx.with_new_scope(|mut scope| {
        let r = scope.eval(
            str.content().encode_utf8_bytes(true).as_ref(),
            "<evalScript>",
            EvalType::Global,
            flags,
        );
        scope.return_with(r)
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
    Ok(JSValue::Bool(unsafe { libc::isatty(fd) != 0 }))
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
    obj.define_property_value_u32(ctx, 0, JSValue::Int(w), JS_PROP_C_W_E)?;
    obj.define_property_value_u32(ctx, 1, JSValue::Int(h), JS_PROP_C_W_E)?;
    Ok(obj.into())
}

pub(super) fn js_ttySetRaw(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let fd = cc.arg(0).to_number(ctx)?.to_i32();

    #[cfg(unix)]
    unsafe {
        static mut OLD_TTY: libc::termios = unsafe { std::mem::zeroed::<libc::termios>() };

        let mut tty = std::mem::zeroed::<libc::termios>();
        libc::tcgetattr(fd, &mut tty);
        OLD_TTY = tty;

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

        extern "C" fn term_exit() {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &raw const OLD_TTY);
            }
        }
        libc::atexit(term_exit);
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
