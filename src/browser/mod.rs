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

pub(crate) use chrome::kill_all_spawned_browsers;

use std::collections::HashMap;

use zuqe::{
    FieldDef, JSValue, NativeObject, Object, ScopedContext,
    gc_lite::{GcTrace, GcTraceCtx},
};

use cdp::CdpClient;

/// Per-runtime browser-side state (JS callbacks), attached to `AikeRuntimeState`
/// so the engine traces it and two runtimes on one thread never share it.
#[derive(Default)]
pub(crate) struct BrowserState {
    /// `handleCertificateError` callback (last wins).
    pub(crate) cert_callback: Option<JSValue>,
    /// CDP client backing the generated `accept` closure.
    pub(crate) cert_client: Option<CdpClient>,
    /// eventId of the certificate error currently being delivered.
    pub(crate) cert_event_id: Option<i64>,
    /// session id -> `onRequestPaused` callback (last wins).
    pub(crate) fetch_callbacks: HashMap<String, JSValue>,
    /// session id -> `page.onLog` callback.
    pub(crate) log_console_callbacks: HashMap<String, Object>,
    /// session id -> `page.onConsole` callback.
    pub(crate) runtime_console_callbacks: HashMap<String, JSValue>,
}

impl GcTrace for BrowserState {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        if let Some(cb) = &self.cert_callback {
            cb.trace(gcx);
        }
        for cb in self.fetch_callbacks.values() {
            cb.trace(gcx);
        }
        for cb in self.log_console_callbacks.values() {
            cb.trace(gcx);
        }
        for cb in self.runtime_console_callbacks.values() {
            cb.trace(gcx);
        }
    }
}

/// Initialize the `browser` JS module (exports the `Browser` class).
pub fn init_mod_browser(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.use_classes(&[
        JSBrowser::class_id(),
        class_browser::JSBrowserProcess::class_id(),
    ])?;

    const MOD_BROWSER_ENTRIES: &[FieldDef] = &[FieldDef::CLASS(
        JSBrowser::CLASS_NAME,
        JSBrowser::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    )];

    ctx.create_module("browser", MOD_BROWSER_ENTRIES)?;
    Ok(())
}
