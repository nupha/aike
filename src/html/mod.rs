// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod parse;

pub(crate) use parse::{HtmlConvertMode, HtmlDocument, HtmlElement, convert_html};

use zuqe::{FieldDef, ScopedContext};

const MOD_HTML_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("parseHTML", 1, parse::js_parse_html, 0),
    FieldDef::FUNC("html2markdown", 1, parse::js_html2markdown, 0),
];

pub fn init_mod_html(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("html", MOD_HTML_ENTRIES)?;
    Ok(())
}
