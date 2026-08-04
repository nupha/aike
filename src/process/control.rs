// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::RefCell;

use zuqe::{
    Array, CallContext, JSResult, JSValue, JobState, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::aike::signal_number;

thread_local! {
    /// Pending `process.nextTick` callbacks, drained on the next microtask.
    static NEXT_TICK_QUEUE: RefCell<Vec<JSValue>> = RefCell::new(Vec::new());
}

/// GC trace: keep pending nextTick callbacks alive.
pub(crate) fn gc_trace_next_tick(gcx: &mut GcTraceCtx) {
    NEXT_TICK_QUEUE.with(|q| {
        for cb in q.borrow().iter() {
            cb.trace(gcx);
        }
    });
}

/// `process.chdir(dir)` — change the current working directory.
pub(super) fn js_chdir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let dir = cc.arg(0).to_string(ctx)?;
    let dir_str = dir.content().encode_utf8(false);
    std::env::set_current_dir(dir_str.as_ref())
        .map_err(|e| ctx.throw_internal_error(&format!("chdir failed: {e}")))?;
    Ok(JSValue::UNDEFINED)
}

/// `process.kill(pid, signal?)` — send a signal to a process.
pub(super) fn js_kill(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = cc.arg(0).to_number(ctx)?.to_i32();
    if pid <= 0 {
        return Err(ctx.throw_type_error("process.kill: invalid pid"));
    }

    // Resolve signal: default SIGTERM
    let sig = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        let arg = cc.arg(1);
        if let Some(s) = arg.as_string(true) {
            let name = s.content().encode_utf8(false);
            signal_number(&name)
                .map(|n| n as i32)
                .unwrap_or(libc::SIGTERM)
        } else {
            arg.to_number(ctx)?.to_i32()
        }
    } else {
        libc::SIGTERM
    };

    #[cfg(unix)]
    {
        let ret = unsafe { libc::kill(pid, sig) };
        if ret != 0 {
            return Err(ctx.throw_internal_error(&format!(
                "process.kill: failed to send signal to pid {pid}"
            )));
        }
    }

    #[cfg(windows)]
    {
        let _ = sig; // Windows only supports terminate
        let ok = unsafe { crate::aike::terminate_process(pid) };
        if !ok {
            return Err(ctx.throw_internal_error(&format!(
                "process.kill: failed to terminate pid {pid}"
            )));
        }
    }

    Ok(JSValue::UNDEFINED)
}

/// `process.nextTick(callback)` — schedule a callback for the next microtask.
pub(super) fn js_nextTick(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let callback = cc.arg(0).ensure_callable(ctx)?;

    let need_microtask = NEXT_TICK_QUEUE.with(|q| q.borrow().is_empty());
    NEXT_TICK_QUEUE.with(|q| q.borrow_mut().push(callback.into()));

    if need_microtask {
        // Queue a job that drains the nextTick queue
        ctx.queue_job(
            |cx| {
                let callbacks: Vec<JSValue> =
                    NEXT_TICK_QUEUE.with(|q| q.borrow_mut().drain(..).collect());
                for cb in callbacks {
                    if cb.call(cx, JSValue::UNDEFINED, &[]).is_err() {
                        // Clear exception to continue processing remaining callbacks
                        cx.take_exception();
                    }
                }
                Ok(JobState::Done)
            },
            (),
        );
    }

    Ok(JSValue::UNDEFINED)
}

/// `process.memoryUsage()` — memory usage statistics.
pub(super) fn js_memoryUsage(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let rss = get_rss_bytes();

    let mut obj = ctx.create_base_object()?;
    obj.define_property_value_str(ctx, "rss", JSValue::from(rss as f64), zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "heapTotal", JSValue::from(0.0), zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "heapUsed", JSValue::from(0.0), zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "external", JSValue::from(0.0), zuqe::JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

/// Get the current resident set size (RSS) in bytes.
fn get_rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // Read VmRSS from /proc/self/status
        if let Ok(content) = std::fs::read_to_string("/proc/self/status") {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb: u64 = rest
                        .trim()
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    return kb * 1024;
                }
            }
        }
        0
    }

    #[cfg(target_os = "macos")]
    {
        // Use getrusage for max RSS (not current, but best available without mach APIs)
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            // ru_maxrss is in bytes on macOS
            return usage.ru_maxrss as u64;
        }
        0
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        unsafe {
            let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ) != 0
            {
                return counters.WorkingSetSize as u64;
            }
        }
        0
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        0
    }
}

/// `process.cpuUsage()` — CPU time usage in microseconds.
pub(super) fn js_cpuUsage(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let (user_us, system_us) = get_cpu_usage_us();

    let mut obj = ctx.create_base_object()?;
    obj.define_property_value_str(ctx, "user", JSValue::from(user_us as f64), zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "system", JSValue::from(system_us as f64), zuqe::JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

/// Get CPU usage in microseconds (user, system).
fn get_cpu_usage_us() -> (u64, u64) {
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            let user = usage.ru_utime.tv_sec as u64 * 1_000_000 + usage.ru_utime.tv_usec as u64;
            let system = usage.ru_stime.tv_sec as u64 * 1_000_000 + usage.ru_stime.tv_usec as u64;
            return (user, system);
        }
        (0, 0)
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

        unsafe {
            let handle = GetCurrentProcess();
            let mut creation: u64 = 0;
            let mut exit: u64 = 0;
            let mut kernel: u64 = 0;
            let mut user: u64 = 0;

            if GetProcessTimes(
                handle,
                &mut creation as *mut u64 as *mut _,
                &mut exit as *mut u64 as *mut _,
                &mut kernel as *mut u64 as *mut _,
                &mut user as *mut u64 as *mut _,
            ) != 0
            {
                // FILETIME is in 100-nanosecond intervals; convert to microseconds
                let user_us = user / 10;
                let kernel_us = kernel / 10;
                return (user_us, kernel_us);
            }
        }
        (0, 0)
    }
}

/// `process.hrtime(time?)` — high-resolution monotonic time.
/// Returns `[seconds, nanoseconds]`.
pub(super) fn js_hrtime(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let now_ns = crate::aike::get_time_ns();

    // If a previous time is provided, compute the difference
    let diff_ns = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        if let Some(arr) = cc.arg(0).as_object_Array() {
            let vals: Vec<JSValue> = arr.payload().values().to_vec();
            if vals.len() >= 2 {
                let prev_s = vals[0].to_number(ctx).map(|n| n.to_f64() as i64).unwrap_or(0);
                let prev_ns = vals[1].to_number(ctx).map(|n| n.to_f64() as i64).unwrap_or(0);
                let prev_total = prev_s * 1_000_000_000 + prev_ns;
                now_ns.saturating_sub(prev_total as u64)
            } else {
                now_ns
            }
        } else {
            now_ns
        }
    } else {
        now_ns
    };

    let secs = diff_ns / 1_000_000_000;
    let nanos = diff_ns % 1_000_000_000;

    let mut arr = Array::create(ctx)?;
    arr.define_property_value_u32(ctx, 0, JSValue::from(secs as f64), zuqe::JS_PROP_C_W_E)?;
    arr.define_property_value_u32(ctx, 1, JSValue::from(nanos as f64), zuqe::JS_PROP_C_W_E)?;

    Ok(arr.into())
}

/// `process.umask(mask?)` — get or set the file mode creation mask.
/// Returns the previous umask. On Windows, always returns 0.
pub(super) fn js_umask(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    #[cfg(unix)]
    {
        if cc.length() > 0 && !cc.arg(0).is_undefined() {
            let mask = cc.arg(0).to_number(ctx)?.to_i32();
            let old = unsafe { libc::umask(mask as libc::mode_t) };
            return Ok(JSValue::from(old as i32));
        } else {
            // Get current umask without changing it: set to 0, then restore
            let old = unsafe { libc::umask(0) };
            unsafe {
                libc::umask(old);
            }
            return Ok(JSValue::from(old as i32));
        }
    }

    #[cfg(windows)]
    {
        let _ = ctx;
        let _ = cc;
        // Windows has no concept of umask
        Ok(JSValue::from(0))
    }
}
