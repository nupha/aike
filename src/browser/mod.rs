// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod cdp;
mod cdp_input;
mod cdp_network;
mod chrome;
mod class_input;
mod class_network;
mod class_page;
mod runtime;

pub use class_page::{JSBrowser, JSPage};

use zuqe::{FieldDef, ScopedContext};

/// Initialize the `browser` JS module.
pub fn init_mod_browser(ctx: &mut ScopedContext) -> Result<(), i32> {
    const MOD_BROWSER_ENTRIES: &[FieldDef] =
        &[FieldDef::FUNC("launch", 1, class_page::js_launch, 0)];
    ctx.create_module("browser", MOD_BROWSER_ENTRIES)?;
    Ok(())
}
