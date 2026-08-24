// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod cdp;
mod cdp_input;
mod cdp_network;
mod chrome;
mod class_browser;
mod class_page;
mod css;
mod dom;
mod emulation;
mod fetch;
mod input;
mod io;
mod log;
mod network;
mod overlay;
mod runtime;
mod security;
mod storage;
mod target;
mod tracing;

pub use {class_browser::JSBrowser, class_page::JSPage};

use zuqe::{gc_lite::GcTrace, gc_lite::GcTraceCtx, FieldDef, NativeObject, ScopedContext};

/// Single GC entry point for the whole browser module: traces every
/// browser-side JS-callback registry so main.rs registers exactly one
/// handler. Statics are pub(crate) precisely so this one function can
/// reach into each submodule directly.
pub(crate) fn gc_trace_browser(gcx: &mut GcTraceCtx) {
    log::CONSOLE_CALLBACK.with(|m| {
        for cb in m.borrow().values() {
            cb.trace(gcx);
        }
    });
    runtime::CONSOLE_CALLBACK.with(|m| {
        for cb in m.borrow().values() {
            cb.trace(gcx);
        }
    });
    security::CERT_CALLBACK.with(|cb| {
        if let Some(v) = cb.borrow().as_ref() {
            v.trace(gcx);
        }
    });
    fetch::FETCH_CALLBACK.with(|m| {
        for cb in m.borrow().values() {
            cb.trace(gcx);
        }
    });
}

/// Initialize the `browser` JS module (exports the `Browser` class).
pub fn init_mod_browser(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.use_classes(&[JSBrowser::class_id()])?;

    const MOD_BROWSER_ENTRIES: &[FieldDef] = &[FieldDef::CLASS(
        JSBrowser::CLASS_NAME,
        JSBrowser::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    )];

    ctx.create_module("browser", MOD_BROWSER_ENTRIES)?;
    Ok(())
}
