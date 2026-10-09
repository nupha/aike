// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Sandboxing for spawned child processes.
//!
//! A [`Policy`] says what the child may do — which paths it may read, write,
//! or run, and whether it has a route out. Three backends enforce it, each
//! with the OS primitive that actually works there: AppContainer on Windows,
//! `sandbox-exec` on macOS, `bwrap` on Linux. None of them can express
//! everything the others can, so what they share is the policy and the
//! precedence rules, not the enforcement.
//!
//! Two invariants hold across all of them, and are enforced in [`Policy::new`]
//! rather than left to a backend:
//!
//! - the working directory is always read-write and nothing else can take
//!   write away from it;
//! - `denied` beats every grant. A grant that `denied` would silently kill is
//!   rejected at build time instead.

// Only the backend for the host is compiled: each one calls OS APIs the
// others' targets do not have, so leaving them ungated would break the build
// everywhere but the platform they were written for.
#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

pub(crate) mod child;
pub(crate) mod path;
pub(crate) mod policy;
pub(crate) mod probe;

mod bindings;

pub(crate) use bindings::{
    get_sandbox_opt, js_sandbox_support, run_sandboxed_sync, spawn_async_sandboxed,
    spawn_sync_sandboxed,
};
pub(crate) use child::SpawnedChild;
pub(crate) use policy::Policy;

use std::{collections::HashMap, path::Path};

/// Spawn `cmd` under `policy`, wired to a fresh set of pipes.
///
/// Each backend supplies its own `spawn`; this is the one place that picks it,
/// so `mod.rs` has a single `#[cfg]` instead of each caller repeating it.
pub(crate) fn spawn(
    policy: &Policy,
    cmd: &str,
    args: &[&str],
    env: &HashMap<String, String>,
) -> Result<SpawnedChild, String> {
    // Resolve the executable before entering the sandbox: the working
    // directory may be mounted differently inside, and PATH lookup against
    // the host's PATH would pick a binary the sandbox cannot reach.
    let program = resolve_program(cmd, policy)?;

    #[cfg(target_os = "macos")]
    {
        macos::spawn(policy, &program, args, env)
    }
    #[cfg(target_os = "linux")]
    {
        linux::spawn(policy, &program, args, env)
    }
    #[cfg(target_os = "windows")]
    {
        windows::spawn(policy, &program, args, env)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (policy, &program, args, env);
        Err("`sandbox` has no backend for this platform".to_owned())
    }
}

/// Turn `cmd` into an absolute path, searching `PATH` when it has no separator.
///
/// The result is resolved on the host, before any sandbox mounts are in place.
fn resolve_program(cmd: &str, policy: &Policy) -> Result<std::path::PathBuf, String> {
    let p = Path::new(cmd);
    if p.components().count() > 1 || cmd.contains(std::path::MAIN_SEPARATOR) {
        return p
            .canonicalize()
            .map_err(|e| format!("`{cmd}` cannot be resolved: {e}"));
    }
    let path_var = std::env::var_os("PATH").ok_or_else(|| {
        format!("`{cmd}` has no path separator and PATH is unset, so it cannot be found")
    })?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(cmd);
        if candidate.is_file()
            && let Ok(real) = candidate.canonicalize()
        {
            return Ok(real);
        }
    }
    let _ = policy;
    Err(format!("`{cmd}` was not found in PATH"))
}
