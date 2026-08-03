// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use zuqe::{FieldDef, NativeObject, ScopedContext};

use crate::aike::JSFileHandle;

use super::{
    dir::{
        FS_COUNT_ALL, FS_COUNT_NON_EMPTY, js_count_lines, js_fs_ensureDir, js_fs_exists,
        js_fs_makeDir, js_fs_readDir, js_fs_remove, js_fs_removeDir, js_fs_stat,
    },
    file::{
        js_fs_appendText, js_fs_open, js_fs_readBytes, js_fs_readJSON, js_fs_readText, js_fs_write,
        js_fs_writeBytes, js_fs_writeJSON, js_fs_writeText,
    },
};

const MOD_FS_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        JSFileHandle::CLASS_NAME,
        JSFileHandle::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    ),
    FieldDef::FUNC("open", 2, js_fs_open, 0),
    FieldDef::FUNC("readText", 1, js_fs_readText, 0),
    FieldDef::FUNC("readJSON", 1, js_fs_readJSON, 0),
    FieldDef::FUNC("readBytes", 1, js_fs_readBytes, 0),
    FieldDef::FUNC("writeText", 2, js_fs_writeText, 0),
    FieldDef::FUNC("writeJSON", 2, js_fs_writeJSON, 0),
    FieldDef::FUNC("countLines", 1, js_count_lines, FS_COUNT_ALL as i16),
    FieldDef::FUNC(
        "countNonEmptyLines",
        1,
        js_count_lines,
        FS_COUNT_NON_EMPTY as i16,
    ),
    FieldDef::FUNC("writeBytes", 2, js_fs_writeBytes, 0),
    FieldDef::FUNC("appendText", 2, js_fs_appendText, 0),
    FieldDef::FUNC("write", 2, js_fs_write, 0),
    FieldDef::FUNC("exists", 1, js_fs_exists, 0),
    FieldDef::FUNC("remove", 1, js_fs_remove, 0),
    FieldDef::FUNC("stat", 1, js_fs_stat, 0),
    FieldDef::FUNC("readDir", 1, js_fs_readDir, 0),
    FieldDef::FUNC("ensureDir", 2, js_fs_ensureDir, 0),
    FieldDef::FUNC("makeDir", 2, js_fs_makeDir, 0),
    FieldDef::FUNC("removeDir", 2, js_fs_removeDir, 0),
];

pub fn init_mod_fs(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("fs", MOD_FS_ENTRIES)?;
    Ok(())
}
