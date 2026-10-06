// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Node.js-compatible `os` module, plus aike's XDG base-directory extensions.
//!
//! Aike extensions live here rather than on the `Aike` namespace, matching how
//! `fs.sha256` / `fs.glob` sit on `fs` rather than on `Aike`.

#[cfg(windows)]
use windows_sys::Win32::System::SystemInformation::{
    GetTickCount64, GetVersionExW, OSVERSIONINFOW,
};

use zuqe::{CallContext, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue, ScopedContext};

use crate::{
    aike::signal::js_signals,
    process::info::{arch_str, platform_str},
};

/// `os.EOL` and `os.devNull` are properties in Node, not functions, and their
/// values are fixed at compile time, so they install as plain string fields.
#[cfg(windows)]
const EOL: &str = "\r\n";
#[cfg(not(windows))]
const EOL: &str = "\n";

/// Node returns the full `\\.\nul` device path on Windows, not the bare `NUL`.
#[cfg(windows)]
const DEV_NULL: &str = "\\\\.\\nul";
#[cfg(not(windows))]
const DEV_NULL: &str = "/dev/null";

/// Create a JS string from a Rust string literal.
fn js_str(ctx: &mut ScopedContext, s: &str) -> Result<JSValue, i32> {
    ctx.create_string_from_utf8(s.as_bytes().to_vec())
        .map(JSValue::make_string)
}

/// `os.homedir()` and the XDG/user-directory lookups share one native
/// handler; `magic` selects which lookup to perform. A lookup that may
/// legitimately be undeterminable on this platform (e.g. `runtimeDir()`
/// off Linux) reports `null` rather than throwing.
fn js_dir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = match cc.magic() {
        DIR_HOME => dirs::home_dir(),
        DIR_TMP => return js_str(ctx, &std::env::temp_dir().to_string_lossy()),
        DIR_CONFIG => dirs::config_dir(),
        DIR_CONFIG_LOCAL => dirs::config_local_dir(),
        DIR_DATA => dirs::data_dir(),
        DIR_DATA_LOCAL => dirs::data_local_dir(),
        DIR_CACHE => dirs::cache_dir(),
        DIR_STATE => dirs::state_dir(),
        DIR_RUNTIME => dirs::runtime_dir(),
        DIR_EXECUTABLE => dirs::executable_dir(),
        DIR_PREFERENCE => dirs::preference_dir(),
        DIR_AUDIO => dirs::audio_dir(),
        DIR_DESKTOP => dirs::desktop_dir(),
        DIR_DOCUMENT => dirs::document_dir(),
        DIR_DOWNLOAD => dirs::download_dir(),
        DIR_FONT => dirs::font_dir(),
        DIR_PICTURE => dirs::picture_dir(),
        DIR_PUBLIC => dirs::public_dir(),
        DIR_TEMPLATE => dirs::template_dir(),
        DIR_VIDEO => dirs::video_dir(),
        _ => unreachable!(),
    };
    match path {
        Some(p) => js_str(ctx, &p.to_string_lossy()),
        None => Ok(JSValue::NULL),
    }
}

// magic constants for js_dir — one native handler, `magic` picks the lookup.
const DIR_HOME: i32 = 0;
const DIR_TMP: i32 = 1;
const DIR_CONFIG: i32 = 2;
const DIR_CONFIG_LOCAL: i32 = 3;
const DIR_DATA: i32 = 4;
const DIR_DATA_LOCAL: i32 = 5;
const DIR_CACHE: i32 = 6;
const DIR_STATE: i32 = 7;
const DIR_RUNTIME: i32 = 8;
const DIR_EXECUTABLE: i32 = 9;
const DIR_PREFERENCE: i32 = 10;
const DIR_AUDIO: i32 = 11;
const DIR_DESKTOP: i32 = 12;
const DIR_DOCUMENT: i32 = 13;
const DIR_DOWNLOAD: i32 = 14;
const DIR_FONT: i32 = 15;
const DIR_PICTURE: i32 = 16;
const DIR_PUBLIC: i32 = 17;
const DIR_TEMPLATE: i32 = 18;
const DIR_VIDEO: i32 = 19;

fn js_arch(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    js_str(ctx, arch_str())
}

fn js_endianness(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    #[cfg(target_endian = "little")]
    {
        js_str(ctx, "LE")
    }
    #[cfg(target_endian = "big")]
    {
        js_str(ctx, "BE")
    }
}

fn js_platform(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    js_str(ctx, platform_str())
}

fn js_type(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // Node capitalizes these ("Linux", not "linux"); keep the full list so
    // `os.type() === "Linux"` holds.
    let t = match std::env::consts::OS {
        "windows" => "Windows_NT",
        "macos" => "Darwin",
        "linux" => "Linux",
        "android" => "Android",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "netbsd" => "NetBSD",
        "dragonfly" => "DragonFlyBSD",
        "solaris" | "illumos" => "SunOS",
        "aix" => "AIX",
        other => other,
    };
    js_str(ctx, t)
}

fn js_available_parallelism(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // The syscall can fail (rlimit/cgroup probing); 1 is the documented floor.
    let n = std::thread::available_parallelism().map_or(1, |n| n.get());
    Ok(JSValue::from(n as i32))
}

/// `os.version()` — the kernel/OS version, *not* the aike runtime version.
/// Mirrors Node: on Windows the product name is read from the registry (plus a
/// service-pack suffix), on POSIX it is the `uname` version field.
fn js_version(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    js_str(ctx, &host_version())
}

/// `os.uptime()` — seconds since the machine booted. A float on Windows
/// (`GetTickCount64/1000`), matching libuv.
fn js_uptime(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let secs = host_uptime();
    Ok(JSValue::from(secs))
}

#[cfg(windows)]
fn host_version() -> String {
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_64KEY, RRF_RT_REG_SZ, RegCloseKey,
        RegGetValueW, RegOpenKeyExW,
    };

    // Major/minor/build come from the version resource; the human-readable
    // product name lives in the registry.
    let mut os_info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    os_info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { GetVersionExW(&mut os_info) } == 0 {
        return String::new();
    }

    let mut product = [0u16; 256];
    let mut product_len = std::mem::size_of_val(&product) as u32;
    let mut key: HKEY = std::ptr::null_mut();
    // "SOFTWARE\Microsoft\Windows NT\CurrentVersion" and "ProductName" as
    // NUL-terminated UTF-16; windows-sys takes raw `*const u16`.
    let subkey: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "ProductName"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Both Win32 calls report success as ERROR_SUCCESS (0), not != 0.
    let ok = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_WOW64_64KEY,
            &mut key,
        ) == 0
            && RegGetValueW(
                key,
                std::ptr::null(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                product.as_mut_ptr().cast(),
                &mut product_len,
            ) == 0
    };
    if ok {
        unsafe { RegCloseKey(key) };
    }

    let name = if ok {
        let n = product
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(product.len());
        String::from_utf16_lossy(&product[..n])
    } else {
        String::new()
    };
    if name.is_empty() {
        return format!(
            "Windows {}.{}.{}",
            os_info.dwMajorVersion, os_info.dwMinorVersion, os_info.dwBuildNumber
        );
    }

    // Append the service pack, e.g. "Windows 10 Pro" -> "Windows 10 Pro SP1".
    let sp_len = os_info
        .szCSDVersion
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(0);
    if sp_len > 0 {
        let sp = String::from_utf16_lossy(&os_info.szCSDVersion[..sp_len]);
        return format!("{name} {sp}");
    }
    name
}

#[cfg(not(windows))]
fn host_version() -> String {
    // `uname` version field, e.g. "#1 SMP Debian 6.1.0-18-amd64".
    let mut buf: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut buf) } != 0 {
        return String::new();
    }
    let bytes: Vec<u8> = buf
        .version
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(windows)]
fn host_uptime() -> f64 {
    // libuv: `GetTickCount64() / 1000.0`
    unsafe { GetTickCount64() as f64 / 1000.0 }
}

#[cfg(target_os = "linux")]
fn host_uptime() -> f64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return 0.0;
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

#[cfg(all(unix, not(target_os = "linux")))]
fn host_uptime() -> f64 {
    // No CLOCK_BOOTTIME on macOS/BSD; sysctl KERN_BOOTTIME gives boot time.
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>() as libc::c_uint as usize;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut boot as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return 0.0;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    (now - boot.tv_sec as f64 - boot.tv_usec as f64 / 1e6).max(0.0)
}

/// `os.constants.priority` — Windows priority classes / POSIX nice values.
#[cfg(windows)]
const PRIORITY: &[(&str, i32)] = &[
    (
        "PRIORITY_LOW",
        windows_sys::Win32::System::Threading::IDLE_PRIORITY_CLASS as i32,
    ),
    (
        "PRIORITY_BELOW_NORMAL",
        windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS as i32,
    ),
    (
        "PRIORITY_NORMAL",
        windows_sys::Win32::System::Threading::NORMAL_PRIORITY_CLASS as i32,
    ),
    (
        "PRIORITY_ABOVE_NORMAL",
        windows_sys::Win32::System::Threading::ABOVE_NORMAL_PRIORITY_CLASS as i32,
    ),
    (
        "PRIORITY_HIGH",
        windows_sys::Win32::System::Threading::HIGH_PRIORITY_CLASS as i32,
    ),
    (
        "PRIORITY_HIGHEST",
        windows_sys::Win32::System::Threading::REALTIME_PRIORITY_CLASS as i32,
    ),
];

#[cfg(not(windows))]
const PRIORITY: &[(&str, i32)] = &[
    ("PRIORITY_LOW", 19),
    ("PRIORITY_BELOW_NORMAL", 10),
    ("PRIORITY_NORMAL", 0),
    ("PRIORITY_ABOVE_NORMAL", -7),
    ("PRIORITY_HIGH", -14),
    ("PRIORITY_HIGHEST", -20),
];

/// `os.constants.dlopen` — POSIX only; empty on Windows, as in Node.
/// `RTLD_DEEPBIND` is glibc-only, so it lives in its own table.
#[cfg(unix)]
const DLOPEN: &[(&str, i32)] = &[
    ("RTLD_LAZY", libc::RTLD_LAZY),
    ("RTLD_NOW", libc::RTLD_NOW),
    ("RTLD_GLOBAL", libc::RTLD_GLOBAL),
    ("RTLD_LOCAL", libc::RTLD_LOCAL),
];

#[cfg(target_os = "linux")]
const DLOPEN_EXTRA: &[(&str, i32)] = &[("RTLD_DEEPBIND", libc::RTLD_DEEPBIND)];
#[cfg(not(target_os = "linux"))]
const DLOPEN_EXTRA: &[(&str, i32)] = &[];

#[cfg(not(unix))]
const DLOPEN: &[(&str, i32)] = &[];

/// Install name → number tables as properties of `obj`.
fn install_int_tables(
    ctx: &mut ScopedContext,
    obj: &mut zuqe::Object,
    tables: &[(&str, &[(&str, i32)])],
) -> Result<(), i32> {
    for &(key, table) in tables {
        let mut sub = ctx.create_base_object()?;
        for &(name, value) in table {
            sub.set_property_str(ctx, name, JSValue::from(value))?;
        }
        obj.set_property_str(ctx, key, sub.into())?;
    }
    Ok(())
}

const MOD_OS_ENTRIES: &[FieldDef] = &[
    // --- platform identity ---
    FieldDef::FUNC("arch", 0, js_arch, 0),
    FieldDef::FUNC("platform", 0, js_platform, 0),
    FieldDef::FUNC("type", 0, js_type, 0),
    FieldDef::FUNC("version", 0, js_version, 0),
    FieldDef::FUNC("uptime", 0, js_uptime, 0),
    // --- host constants ---
    FieldDef::STR("EOL", EOL, JS_PROP_ENUMERABLE),
    FieldDef::STR("devNull", DEV_NULL, JS_PROP_ENUMERABLE),
    FieldDef::FUNC("endianness", 0, js_endianness, 0),
    FieldDef::FUNC("availableParallelism", 0, js_available_parallelism, 0),
    // `os.constants` is a plain nested namespace. `errno` is not populated —
    // aike does not surface C errno values to JS.
    FieldDef::LAZY_VALUE(
        "constants",
        |cx| {
            let mut obj = cx.create_base_object()?;
            let signals = js_signals(cx)?;
            obj.set_property_str(cx, "signals", signals)?;
            install_int_tables(cx, &mut obj, &[("priority", PRIORITY)])?;
            // `dlopen` is a single object; RTLD_DEEPBIND is merged in rather
            // than exposed under a name Node does not have.
            let mut dlopen = cx.create_base_object()?;
            for &(name, value) in DLOPEN.iter().chain(DLOPEN_EXTRA) {
                dlopen.set_property_str(cx, name, JSValue::from(value))?;
            }
            obj.set_property_str(cx, "dlopen", dlopen.into())?;
            Ok(obj.into())
        },
        JS_PROP_ENUMERABLE,
    ),
    // --- directories --- (single native handler, magic selects the lookup)
    FieldDef::FUNC("homedir", 0, js_dir, DIR_HOME as _),
    FieldDef::FUNC("tmpdir", 0, js_dir, DIR_TMP as _),
    FieldDef::FUNC("configDir", 0, js_dir, DIR_CONFIG as _),
    FieldDef::FUNC("configLocalDir", 0, js_dir, DIR_CONFIG_LOCAL as _),
    FieldDef::FUNC("dataDir", 0, js_dir, DIR_DATA as _),
    FieldDef::FUNC("dataLocalDir", 0, js_dir, DIR_DATA_LOCAL as _),
    FieldDef::FUNC("cacheDir", 0, js_dir, DIR_CACHE as _),
    FieldDef::FUNC("stateDir", 0, js_dir, DIR_STATE as _),
    FieldDef::FUNC("runtimeDir", 0, js_dir, DIR_RUNTIME as _),
    FieldDef::FUNC("executableDir", 0, js_dir, DIR_EXECUTABLE as _),
    FieldDef::FUNC("preferenceDir", 0, js_dir, DIR_PREFERENCE as _),
    FieldDef::FUNC("audioDir", 0, js_dir, DIR_AUDIO as _),
    FieldDef::FUNC("desktopDir", 0, js_dir, DIR_DESKTOP as _),
    FieldDef::FUNC("documentDir", 0, js_dir, DIR_DOCUMENT as _),
    FieldDef::FUNC("downloadDir", 0, js_dir, DIR_DOWNLOAD as _),
    FieldDef::FUNC("fontDir", 0, js_dir, DIR_FONT as _),
    FieldDef::FUNC("pictureDir", 0, js_dir, DIR_PICTURE as _),
    FieldDef::FUNC("publicDir", 0, js_dir, DIR_PUBLIC as _),
    FieldDef::FUNC("templateDir", 0, js_dir, DIR_TEMPLATE as _),
    FieldDef::FUNC("videoDir", 0, js_dir, DIR_VIDEO as _),
];

pub fn init_mod_os(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("os", MOD_OS_ENTRIES)?;
    Ok(())
}
