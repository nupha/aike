// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod class_jsonl_array;
mod funcs;

use class_jsonl_array::JSJSONLArray;

use zuqe::{FieldDef, JS_PROP_ENUMERABLE, NativeObject, ScopedContext};

const JSONL_MOD_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS("JSONLArray", JSJSONLArray::class_id, JS_PROP_ENUMERABLE),
    FieldDef::FUNC("parse", 1, funcs::js_parse, 0),
    FieldDef::FUNC("tryParse", 1, funcs::js_try_parse, 0),
    FieldDef::FUNC("stringify", 1, funcs::js_stringify, 0),
    FieldDef::FUNC("readFromFile", 1, funcs::js_read_from_file, 0),
    FieldDef::FUNC("appendToFile", 1, funcs::js_append_to_file, 0),
    FieldDef::FUNC("parseChunk", 1, funcs::js_parse_chunk, 0),
    FieldDef::FUNC("parseStream", 1, funcs::js_parse_stream, 0),
];

pub fn init_mod_jsonl(cx: &mut ScopedContext) -> Result<(), i32> {
    cx.use_classes(&[JSJSONLArray::class_id()])?;
    cx.create_module("jsonl", JSONL_MOD_ENTRIES)?;
    Ok(())
}
