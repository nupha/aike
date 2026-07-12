// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod cdp;
mod cdp_input;
mod cdp_network;
mod chrome;
mod class_browser;
mod class_page;
mod dom;
mod emulation;
mod fetch;
mod input;
mod log;
mod network;
mod runtime;
mod security;
mod target;

pub use {class_browser::JSBrowser, class_page::JSPage};

use zuqe::{FieldDef, NativeObject, ScopedContext};

/// Initialize the `browser` JS module (exports the `Browser` class).
pub fn init_mod_browser(ctx: &mut ScopedContext) -> Result<(), i32> {
    const MOD_BROWSER_ENTRIES: &[FieldDef] = &[FieldDef::CLASS(
        "Browser",
        JSBrowser::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    )];
    ctx.create_module("browser", MOD_BROWSER_ENTRIES)?;
    Ok(())
}
