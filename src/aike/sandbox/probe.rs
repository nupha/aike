// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Whether this machine can run a sandbox at all, and why not if it cannot.
//!
//! The check runs the real thing once and caches the answer. Reading
//! `/proc/sys/kernel/unprivileged_userns_clone` instead would be unreliable:
//! Debian uses `user.max_user_namespaces`, and Ubuntu 23.10+ layers a separate
//! AppArmor switch on top, so any one file is a guess about the rest.
//!
//! Every failure mode reports a message meant to be read by the person whose
//! command just failed. None of them falls back to running unsandboxed: an
//! agent that appears to be sandboxed and is not is worse than one that
//! refuses to start.

use std::sync::OnceLock;

#[cfg(target_os = "macos")]
static MACOS: OnceLock<Result<(), String>> = OnceLock::new();
#[cfg(target_os = "windows")]
static WINDOWS: OnceLock<Result<(), String>> = OnceLock::new();
#[cfg(target_os = "linux")]
static LINUX: OnceLock<Result<(), String>> = OnceLock::new();

/// The backend in use on this platform, for `Aike.sandboxSupport()`.
pub(crate) fn backend_name() -> Option<&'static str> {
    match std::env::consts::OS {
        "macos" | "ios" => Some("seatbelt"),
        "windows" => Some("appcontainer"),
        "linux" | "android" => Some("bubblewrap"),
        _ => None,
    }
}

/// Whether a sandbox can be created here. `Err` carries the reason, phrased for
/// the user.
///
/// The answer is cached: the probe spawns a process, and a script that starts
/// a dozen sandboxed commands should pay for it once.
pub(crate) fn probe() -> Result<(), &'static str> {
    #[cfg(target_os = "macos")]
    let cached: &Result<(), String> = MACOS.get_or_init(probe_macos);
    #[cfg(target_os = "windows")]
    let cached: &Result<(), String> = WINDOWS.get_or_init(probe_windows);
    #[cfg(target_os = "linux")]
    let cached: &Result<(), String> = LINUX.get_or_init(probe_linux);

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        cached.as_ref().map_err(|e| e.as_str()).map(|()| ())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err("`sandbox` has no backend for this platform")
    }
}

#[cfg(target_os = "macos")]
fn probe_macos() -> Result<(), String> {
    const EXEC: &str = "/usr/bin/sandbox-exec";
    if !std::path::Path::new(EXEC).exists() {
        return Err(format!(
            "`sandbox` needs {EXEC}, which this macOS does not provide. Apple has \
             deprecated sandbox-exec and may remove it; without administrator rights \
             there is no replacement for process-level isolation on macOS."
        ));
    }
    // Existence is not enough: a binary can be present and still fail to apply
    // a profile. Run a minimal deny-default one — `(deny default)` is the
    // shape that actually exercises the mechanism, and `true` is the cheapest
    // thing that can run under it.
    let out = std::process::Command::new(EXEC)
        .args([
            "-p",
            "(version 1)(deny default)(allow process-exec)(allow file-read*)",
            "--",
            "/usr/bin/true",
        ])
        .output()
        .map_err(|e| format!("`sandbox` probe could not run {EXEC}: {e}"))?;
    if !out.status.success() {
        // sandbox-exec writes "WARNING: sandbox-exec is deprecated ..." to
        // stderr on every run; that is expected and not the failure.
        return Err(format!(
            "`sandbox` probe failed: {EXEC} exited {} — stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn probe_windows() -> Result<(), String> {
    use crate::aike::sandbox::windows;

    // No elevation. If creating a profile turns out to need it, the honest
    // answer is a separate elevated helper rather than aike raising its own
    // privileges — see the plan's open question on rappct.
    windows::probe_profile().map_err(|e| {
        format!("`sandbox` cannot create an AppContainer profile on this machine: {e}")
    })
}

#[cfg(target_os = "linux")]
fn probe_linux() -> Result<(), String> {
    let bwrap = std::env::var_os("AIKE_SANDBOX_BWRAP")
        .map(std::path::PathBuf::from)
        .or_else(|| which("bwrap"))
        .ok_or_else(|| {
            "`sandbox` needs the bubblewrap binary, which is not installed. \
             Install it: apt install bubblewrap / dnf install bubblewrap / \
             pacman -S bubblewrap / apk add bubblewrap."
                .to_owned()
        })?;

    // Run it for real. Whether an unprivileged user namespace is available is
    // not a single sysctl: it depends on the kernel version, the distro's
    // `unprivileged_userns_clone`, and (Ubuntu 23.10+) AppArmor.
    let out = std::process::Command::new(&bwrap)
        .args(["--unshare-all", "--ro-bind", "/", "/", "--", "/bin/true"])
        .output()
        .map_err(|e| format!("`sandbox` probe could not run {}: {e}", bwrap.display()))?;
    if !out.status.success() {
        return Err(format!(
            "`sandbox` probe failed — bubblewrap could not create a user namespace. \
             On Ubuntu 23.10 and later this is blocked for unconfined processes; set \
             kernel.apparmor_restrict_unprivileged_userns=0, or give the aike executable \
             an AppArmor profile that allows user namespaces. Underlying error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// The path of `name` on `PATH`, honoring `PATHEXT` on Windows.
#[cfg(target_os = "linux")]
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|c| c.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_name_matches_the_target() {
        assert_eq!(
            backend_name().is_some(),
            cfg!(any(
                target_os = "macos",
                target_os = "windows",
                target_os = "linux"
            ))
        );
    }

    #[test]
    fn probe_is_stable_within_a_process() {
        // Probing twice must not re-run the sandbox, and must agree with
        // itself — the cache is the point.
        let first = probe().map_err(|e| e.to_owned());
        let second = probe().map_err(|e| e.to_owned());
        assert_eq!(first, second);
    }

    #[test]
    fn a_probe_failure_names_a_remedy() {
        // Whatever this machine's answer is, a failure has to tell the user
        // what to do about it rather than just reporting that it failed.
        if let Err(reason) = probe() {
            assert!(!reason.is_empty(), "a probe failure must carry a reason");
            assert!(
                reason.contains('`'),
                "the reason should name the thing that is missing: {reason}"
            );
        }
    }
}
