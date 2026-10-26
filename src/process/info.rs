// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use std::{sync::LazyLock, time::Instant};

use zuqe::{
    CallContext, JS_PROP_ENUMERABLE, JSResult, JSValue, ScopedContext,
    zapi::{ZQ_ABI_MIN, ZQ_ABI_VERSION, ZQ_API_MIN, ZQ_API_VERSION},
};

/// Process start time — used by `process.uptime()`.
static START_TIME: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Stored process title — used by `process.title` getter/setter.
static PROCESS_TITLE: LazyLock<std::sync::Mutex<String>> =
    LazyLock::new(|| std::sync::Mutex::new(String::new()));

/// Map Rust `std::env::consts::ARCH` to Node.js-style architecture names.
///
/// Not a `const fn` — `str` matching is not allowed in const context, so
/// `FieldDef::STR` callers must reach this through `LAZY_VALUE`.
pub(crate) fn arch_str() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    }
}

/// Map Rust `std::env::consts::OS` to Node.js-style platform names.
///
/// Shared by `process.platform` and `os.platform()` — Node documents the two
/// as equivalent, so the mapping must not drift apart.
pub(crate) fn platform_str() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

/// Runtime version string, e.g. `"v0.1.0"`.
pub(crate) const VERSION_STR: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// The `-V` / `--version` banner: runtime version, the git commit it was
/// built from, plus the zapi ABI/API contract this build speaks (and the
/// lowest it still accepts).
///
/// Leaked to `'static` because clap wants a `&'static str`; there is exactly
/// one, so the leak is bounded.
pub(crate) fn version_banner() -> &'static str {
    Box::leak(
        format!(
            "{ver} ({hash}, {time}), with zapi abi {ZQ_ABI_VERSION}, api {ZQ_API_VERSION} \
             (min abi {ZQ_ABI_MIN}, min api {ZQ_API_MIN})",
            ver = env!("CARGO_PKG_VERSION"),
            hash = env!("AIKE_GIT_HASH"),
            time = env!("AIKE_GIT_TIME"),
        )
        .into_boxed_str(),
    )
}

/// Get the parent process ID.
pub(super) fn get_ppid() -> i32 {
    #[cfg(unix)]
    {
        unsafe { libc::getppid() }
    }

    #[cfg(windows)]
    {
        get_ppid_windows()
    }
}

/// Windows: find parent PID via CreateToolhelp32Snapshot.
#[cfg(windows)]
fn get_ppid_windows() -> i32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let current_pid = unsafe { GetCurrentProcessId() };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() {
            return 0;
        }

        let mut entry: PROCESSENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;

        if Process32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == current_pid {
                    let ppid = entry.th32ParentProcessID as i32;
                    CloseHandle(snapshot);
                    return ppid;
                }
                if Process32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    0
}

/// `process.versions` — version info object.
pub(super) fn js_versions(ctx: &mut ScopedContext) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;

    let aike_ver = ctx.create_string_from_utf8(env!("CARGO_PKG_VERSION").as_bytes().to_vec())?;
    obj.define_property_value_str(
        ctx,
        "aike",
        JSValue::make_string(aike_ver),
        JS_PROP_ENUMERABLE,
    )?;

    Ok(obj.into())
}

/// `process.title` getter.
pub(super) fn js_title_get(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let title = PROCESS_TITLE.lock().unwrap().clone();
    if title.is_empty() {
        // Default: return the executable name
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_else(|| "aike".to_string());
        ctx.create_string_from_utf8(exe.into_bytes())
            .map(JSValue::make_string)
    } else {
        ctx.create_string_from_utf8(title.into_bytes())
            .map(JSValue::make_string)
    }
}

/// `process.title` setter.
pub(super) fn js_title_set(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let title = cc.arg(0).to_string(ctx)?;
    let title_str = title.content().encode_utf8(false).to_string();

    *PROCESS_TITLE.lock().unwrap() = title_str.clone();

    // Set the OS-level process title
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::SetConsoleTitleW;
        let wide: Vec<u16> = title_str.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            SetConsoleTitleW(wide.as_ptr());
        }
    }

    #[cfg(target_os = "linux")]
    {
        // prctl(PR_SET_NAME) sets the thread/process name (visible in ps/top)
        let mut buf = [0i8; 16];
        let bytes = title_str.as_bytes();
        let len = bytes.len().min(15);
        for (i, &b) in bytes[..len].iter().enumerate() {
            buf[i] = b as i8;
        }
        unsafe {
            libc::prctl(libc::PR_SET_NAME, buf.as_ptr() as usize, 0, 0, 0);
        }
    }

    Ok(JSValue::UNDEFINED)
}

/// `process.uptime()` — seconds since process start.
pub(super) fn js_uptime(_: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let elapsed = START_TIME.elapsed().as_secs_f64();
    Ok(JSValue::from(elapsed))
}
