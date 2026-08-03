// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use zuqe::{Array, CallContext, JSResult, JSValue, ScopedContext};

use crate::aike::make_stats;

use super::file::{get_path_arg, spawn_fs_op, spawn_fs_void};

/// Parse an optional `{ recursive?: boolean }` options object from JS argument 1.
/// A missing options object falls back to `default`. With `strict`, a non-object
/// is a type error; otherwise it is treated as `false`.
fn recursive_opt(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    default: bool,
    strict: bool,
) -> Result<bool, i32> {
    let opts = cc.arg(1);
    if opts.is_undefined() {
        return Ok(default);
    }
    if !opts.is_object() {
        return if strict {
            Err(ctx.throw_type_error("options must be an object"))
        } else {
            Ok(false)
        };
    }
    opts.get_property_str(ctx, "recursive")?.to_bool()
}

// ---------------------------------------------------------------------------
// ensureDir - ensure directory exists (idempotent, default recursive)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_ensureDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let recursive = recursive_opt(ctx, &cc, true, true)?;
    spawn_fs_void(ctx, async move {
        let r = if recursive {
            tokio::fs::create_dir_all(&path).await
        } else {
            tokio::fs::create_dir(&path).await
        };
        r.map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// Simple dirent data sent through channel (metadata collected in background)
// ---------------------------------------------------------------------------
struct DirentData {
    name: String,
    size: u64,
    is_file: bool,
    is_dir: bool,
    is_symlink: bool,
}

fn make_dirent_obj(ctx: &mut ScopedContext, d: &DirentData) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;

    let name_s = ctx.create_string_from_utf8(d.name.as_bytes().to_vec())?;
    obj.define_property_value_str(
        ctx,
        "name",
        JSValue::make_string(name_s),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    obj.define_property_value_str(
        ctx,
        "isFile",
        JSValue::from(d.is_file),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isDirectory",
        JSValue::from(d.is_dir),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isSymbolicLink",
        JSValue::from(d.is_symlink),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    let size = JSValue::from(d.size as i64);
    obj.define_property_value_str(ctx, "size", size, zuqe::JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

// ---------------------------------------------------------------------------
// readDir - read directory contents
// ---------------------------------------------------------------------------
pub(super) fn js_fs_readDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            let mut dir = tokio::fs::read_dir(&path)
                .await
                .map_err(|e| e.to_string())?;

            let mut entries = Vec::new();
            while let Some(entry) = dir.next_entry().await.map_err(|e| e.to_string())? {
                let name = entry.file_name().to_string_lossy().to_string();
                let ft = entry.file_type().await.map_err(|e| e.to_string())?;
                let is_symlink = ft.is_symlink();

                // TODO: restrict symlink access for chroot security
                if is_symlink {
                    eprintln!("warning: symlink '{}' in readDir may escape chroot", name);
                }

                let meta = entry.metadata().await.map_err(|e| e.to_string())?;
                entries.push(DirentData {
                    name,
                    size: meta.len(),
                    is_file: meta.is_file(),
                    is_dir: meta.is_dir(),
                    is_symlink,
                });
            }

            Ok(entries)
        },
        |cx, entries| {
            let mut arr = Vec::<JSValue>::with_capacity(entries.len());
            for d in &entries {
                arr.push(make_dirent_obj(cx, d)?);
            }
            Array::from_values(cx, arr).map(JSValue::from)
        },
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// exists - check if a path exists
// ---------------------------------------------------------------------------
pub(super) fn js_fs_exists(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            tokio::fs::try_exists(&path)
                .await
                .map_err(|e| e.to_string())
        },
        |_cx, exists| Ok(JSValue::from(exists)),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// makeDir - create a directory
//   makeDir(path)                    – non-recursive (fails if parent missing)
//   makeDir(path, { recursive: true }) – recursive (like mkdir -p)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_makeDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let recursive = recursive_opt(ctx, &cc, false, false)?;
    spawn_fs_void(ctx, async move {
        let r = if recursive {
            tokio::fs::create_dir_all(&path).await
        } else {
            tokio::fs::create_dir(&path).await
        };
        r.map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// removeDir - remove a directory
//   removeDir(path)                        – remove an empty directory
//   removeDir(path, { recursive: true })   – recursively remove a directory tree
// ---------------------------------------------------------------------------
pub(super) fn js_fs_removeDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let recursive = recursive_opt(ctx, &cc, false, true)?;
    spawn_fs_void(ctx, async move {
        let r = if recursive {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_dir(&path).await
        };
        r.map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// remove - delete a file or symbolic link (POSIX unlink semantics)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_remove(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_void(ctx, async move {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

pub(super) fn js_fs_stat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            tokio::fs::metadata(&path).await.map_err(|e| e.to_string())
        },
        |cx, meta| make_stats(cx, &meta),
    )
    .map(JSValue::from)
}

pub(super) const FS_COUNT_ALL: i32 = 0;
pub(super) const FS_COUNT_NON_EMPTY: i32 = 1;

/// Backend for `countLines` and `countNonEmptyLines` (registered with a
/// different magic const). Missing/unreadable files resolve to 0 rather than
/// rejecting the promise.
pub(super) fn js_count_lines(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(cx, &cc)?;
    let non_empty = cc.magic() == FS_COUNT_NON_EMPTY;
    spawn_fs_op(
        cx,
        async move {
            let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            let count = if non_empty {
                text.lines().filter(|l| !l.trim().is_empty()).count()
            } else {
                text.lines().count()
            };
            Ok(count as i64)
        },
        |_cx, count| Ok(JSValue::from(count)),
    )
    .map(JSValue::from)
}
