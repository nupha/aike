// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG standard API implementations for Aike runtime.
//!
//! This module groups all WHATWG-standard Web API implementations:
//! - Console (Console Standard)
//! - Timers (setTimeout/setInterval — Timers Standard)
//! - TextEncoder / TextDecoder (Encoding Standard)
//! - URL / URLSearchParams (URL Standard)
//! - AbortController / AbortSignal (DOM Standard)

pub mod abort;
pub mod console;
pub mod text_decoder;
pub mod text_encoder;
pub mod timeout;
pub mod url;

use zuqe::{Object, ScopedContext};

/// Install all WHATWG-standard global APIs.
///
/// This registers:
/// - Console (console.log, etc.)
/// - Timers (setTimeout, setInterval, clearTimeout, clearInterval)
/// - URL classes (URL, URLSearchParams — registered via distributed_slice)
/// - Abort classes (AbortController, AbortSignal — registered via distributed_slice)
/// - Encoding classes (TextEncoder, TextDecoder — registered via distributed_slice)
///
/// The class constructors (TextEncoder, TextDecoder, URL, URLSearchParams,
/// AbortController, AbortSignal) are automatically exposed to globalThis
/// via `expose_to_global()` in the `def_native_object!` macro.
/// Only non-class globals (console, timers, static methods) are installed here.
pub(crate) fn install_whatwg(ctx: &mut ScopedContext, mut global: Object) -> Result<(), i32> {
    console::install_console(ctx, &mut global)?;
    timeout::install_timeout(ctx, global)?;
    abort::install_abort_statics(ctx, &mut global)?;
    Ok(())
}
