// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Bakes the build timestamp into the binary for `aike -V`.
//!
//! `SOURCE_DATE_EPOCH` wins over the wall clock so release builds stay
//! reproducible.

use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

fn main() {
    // Without this, `SOURCE_DATE_EPOCH` is invisible to Cargo's build-script
    // fingerprint: flipping it replays the cached output instead of stamping
    // the new value, silently defeating reproducible release builds.
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let epoch = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                // A pre-1970 clock yields 0, which the formatter renders as
                // the epoch rather than failing the build.
                .unwrap_or(0)
        });

    println!("cargo::rustc-env=AIKE_BUILD_EPOCH={epoch}");
}
