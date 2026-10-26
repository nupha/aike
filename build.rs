// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

// Embed the git commit hash and commit time into the binary so the
// `--version` banner can identify the exact build. Fails soft: outside a git
// checkout (e.g. a source tarball) the values fall back to "unknown".
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    // Rebuild when HEAD moves or the checked-out ref is updated (new commit).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let hash = git(&["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    let time = git(&["log", "-1", "--format=%cI"])
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AIKE_GIT_HASH={hash}");
    println!("cargo:rustc-env=AIKE_GIT_TIME={time}");
}
