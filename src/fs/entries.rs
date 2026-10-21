// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use zuqe::{FieldDef, ModuleDef, NativeObject, ScopedContext};

use crate::aike::JSFileHandle;

use super::{
    dir::{
        FS_COUNT_ALL, FS_COUNT_NON_EMPTY, js_count_lines, js_fs_ensureDir, js_fs_exists,
        js_fs_makeDir, js_fs_readDir, js_fs_remove, js_fs_removeDir, js_fs_stat,
    },
    file::{
        FS_READ_BYTES, FS_READ_TEXT, FS_REPLACE_ALL, FS_REPLACE_UNIQUE, FS_WRITE_BYTES,
        FS_WRITE_TEXT, js_fs_appendText, js_fs_open, js_fs_read, js_fs_readJSON, js_fs_realpath,
        js_fs_replaceText, js_fs_sha256, js_fs_write, js_fs_write_auto, js_fs_writeJSON,
    },
    glob::js_fs_glob,
    grep::js_fs_grep,
};

const MOD_FS_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        JSFileHandle::CLASS_NAME,
        JSFileHandle::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    ),
    FieldDef::FUNC("open", 2, js_fs_open, 0),
    FieldDef::FUNC("realpath", 1, js_fs_realpath, 0),
    FieldDef::FUNC("sha256", 1, js_fs_sha256, 0),
    FieldDef::FUNC("glob", 1, js_fs_glob, 0),
    FieldDef::FUNC("grep", 1, js_fs_grep, 0),
    FieldDef::FUNC("readText", 1, js_fs_read, FS_READ_TEXT as i16),
    FieldDef::FUNC("readJSON", 1, js_fs_readJSON, 0),
    FieldDef::FUNC("readBytes", 1, js_fs_read, FS_READ_BYTES as i16),
    FieldDef::FUNC("writeText", 2, js_fs_write, FS_WRITE_TEXT as i16),
    FieldDef::FUNC("writeJSON", 2, js_fs_writeJSON, 0),
    FieldDef::FUNC("countLines", 1, js_count_lines, FS_COUNT_ALL as i16),
    FieldDef::FUNC(
        "countNonEmptyLines",
        1,
        js_count_lines,
        FS_COUNT_NON_EMPTY as i16,
    ),
    FieldDef::FUNC("writeBytes", 2, js_fs_write, FS_WRITE_BYTES as i16),
    FieldDef::FUNC("appendText", 2, js_fs_appendText, 0),
    FieldDef::FUNC(
        "replaceTextUnique",
        3,
        js_fs_replaceText,
        FS_REPLACE_UNIQUE as i16,
    ),
    FieldDef::FUNC(
        "replaceTextAll",
        3,
        js_fs_replaceText,
        FS_REPLACE_ALL as i16,
    ),
    FieldDef::FUNC("write", 2, js_fs_write_auto, 0),
    FieldDef::FUNC("exists", 1, js_fs_exists, 0),
    FieldDef::FUNC("remove", 1, js_fs_remove, 0),
    FieldDef::FUNC("stat", 1, js_fs_stat, 0),
    FieldDef::FUNC("readDir", 1, js_fs_readDir, 0),
    FieldDef::FUNC("ensureDir", 2, js_fs_ensureDir, 0),
    FieldDef::FUNC("makeDir", 2, js_fs_makeDir, 0),
    FieldDef::FUNC("removeDir", 2, js_fs_removeDir, 0),
];

pub fn install_mod_fs(ctx: &mut ScopedContext) -> Result<ModuleDef, i32> {
    ctx.use_classes(&[JSFileHandle::class_id()])?;

    ctx.create_module("fs", MOD_FS_ENTRIES)
}
