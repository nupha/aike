// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Windows backend: an AppContainer whose package SID is granted per-path ACL
//! entries.
//!
//! An AppContainer token is default-deny against the filesystem: it reaches
//! only paths whose ACL names its package SID. That is the same shape the other
//! two backends express, so a configured path is granted one entry at a time
//! and nothing else is reachable.
//!
//! It has to be in-process. The capability set is attached to the child at
//! creation time through `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, so
//! there is no "exec.exe" equivalent to shell out to — a host that wanted one
//! would have to write a relay that proxies stdio and reaps the child.
//!
//! ⚠️ **Not yet run on real Windows.** This compiles against `rappct`'s API
//! but has never been executed; see the plan's open question about whether
//! creating a profile requires elevation.

use std::{collections::HashMap, path::Path};

use super::{
    child::{ChildControl, SpawnedChild, WaitResult},
    policy::{NetworkPolicy, Policy},
};

pub(crate) fn spawn(
    policy: &Policy,
    program: &Path,
    args: &[&str],
    env: &HashMap<String, String>,
) -> Result<SpawnedChild, String> {
    let _ = (policy, program, args, env, NetworkPolicy::DenyAll);
    Err("`sandbox` on Windows is not implemented yet".to_owned())
}

/// Whether an AppContainer profile can be created without elevation.
pub(crate) fn probe_profile() -> Result<(), String> {
    Err("`sandbox` on Windows is not implemented yet".to_owned())
}

#[allow(dead_code)]
fn unused(_: ChildControl, _: SpawnedChild, _: WaitResult) {}
