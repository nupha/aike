// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

mod parse;

pub(crate) use parse::{HtmlDocument, HtmlElement};

use zuqe::{FieldDef, ModuleDef, NativeObject, ScopedContext};

const MOD_HTML_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("parseHTML", 1, parse::js_parse_html, 0),
    FieldDef::FUNC("html2markdown", 1, parse::js_html2markdown, 0),
];

pub fn install_mod_html(ctx: &mut ScopedContext) -> Result<ModuleDef, i32> {
    ctx.use_classes(&[HtmlDocument::class_id(), HtmlElement::class_id()])?;

    ctx.create_module("html", MOD_HTML_ENTRIES)
}
