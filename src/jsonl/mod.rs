// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

pub(crate) mod funcs;

use zuqe::{FieldDef, ScopedContext};

const JSONL_MOD_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("parse", 1, funcs::js_parse, 0),
    FieldDef::FUNC("stringify", 1, funcs::js_stringify, 0),
    FieldDef::FUNC("read", 1, funcs::js_read_jsonl, 0),
    FieldDef::FUNC(
        "append",
        1,
        funcs::js_write_jsonl,
        funcs::JSONL_WRITE_APPEND as i16,
    ),
    FieldDef::FUNC(
        "write",
        1,
        funcs::js_write_jsonl,
        funcs::JSONL_WRITE_REWRITE as i16,
    ),
    FieldDef::FUNC("parseChunk", 1, funcs::js_parse_chunk, 0),
    FieldDef::FUNC("parseStream", 1, funcs::js_parse_stream, 0),
];

pub fn init_mod_jsonl(cx: &mut ScopedContext) -> Result<(), i32> {
    cx.create_module("jsonl", JSONL_MOD_ENTRIES)?;
    Ok(())
}
