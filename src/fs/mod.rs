// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

mod dir;
mod entries;
mod file;
mod glob;
mod grep;

pub use entries::init_mod_fs;
pub(crate) use file::write_file_atomic;

use zuqe::{CallContext, ScopedContext};

// ---- shared options-object readers (fs.glob / fs.grep opts arg) ----

/// Read an optional string property from the JS options object at `cc.arg(1)`.
fn opt_str(ctx: &mut ScopedContext, cc: &CallContext, name: &str) -> Result<Option<String>, i32> {
    let v = cc.arg(1).get_property_str(ctx, name)?;
    if v.is_undefined() {
        Ok(None)
    } else {
        let s = v.to_string(ctx)?;
        Ok(Some(s.content().encode_utf8(false).to_string()))
    }
}

/// Read an optional boolean property, falling back to `default`.
fn read_bool(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    name: &str,
    default: bool,
) -> Result<bool, i32> {
    let v = cc.arg(1).get_property_str(ctx, name)?;
    if v.is_undefined() {
        Ok(default)
    } else {
        v.to_bool()
    }
}

/// Read an optional string-array property.
fn opt_str_array(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    name: &str,
) -> Result<Vec<String>, i32> {
    let v = cc.arg(1).get_property_str(ctx, name)?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_object_Array() {
        for item in arr.payload().values() {
            if let Some(s) = item.as_string(true) {
                out.push(s.content().encode_utf8(false).to_string());
            }
        }
    }
    Ok(out)
}
