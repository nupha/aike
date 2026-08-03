// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::path::PathBuf;

use zuqe::{
    Array, CallContext, JSResult, JSValue, PollSender, ScopedContext,
    builtins::promise::Promise,
};

use crate::aike::make_stats;

use super::file::make_void_promise;

// ---------------------------------------------------------------------------
// ensureDir - ensure directory exists (idempotent, default recursive)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_ensureDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).as_ref());

    // Parse optional options object: ensureDir(path, { recursive?: boolean })
    let opts = cc.arg(1);
    let recursive = if !opts.is_undefined() {
        if opts.is_object() {
            opts.get_property_str(ctx, "recursive")?.to_bool()?
        } else {
            return Err(ctx.throw_type_error("options must be an object"));
        }
    } else {
        true
    };

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        let r = if recursive {
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|e| e.to_string())
        } else {
            tokio::fs::create_dir(&path)
                .await
                .map_err(|e| e.to_string())
        };

        let _ = PollSender::from(tx).send_async(r).await;
    });

    Ok(promise.into())
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
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) = Promise::create_with_channel::<Result<Vec<DirentData>, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let entries = payload.map_err(|e| cx.throw_internal_error(&e))?;

            let mut arr = Vec::<JSValue>::with_capacity(entries.len());
            for d in &entries {
                arr.push(make_dirent_obj(cx, d)?);
            }
            Array::from_values(cx, arr).map(JSValue::from)
        },
    )?;

    tokio::spawn(async move {
        let r = async {
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
        }
        .await;

        let _ = PollSender::from(tx).send_async(r).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// exists - check if a path exists
// ---------------------------------------------------------------------------
pub(super) fn js_fs_exists(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) =
        Promise::create_with_channel::<Result<bool, String>>(ctx, |cx, payload| -> JSResult {
            let exists = payload.map_err(|e| cx.throw_internal_error(&e))?;
            Ok(JSValue::from(exists))
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::try_exists(&path)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// makeDir - create a directory
//   makeDir(path)                    – non-recursive (fails if parent missing)
//   makeDir(path, { recursive: true }) – recursive (like mkdir -p)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_makeDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    // Parse optional options: makeDir(path, { recursive?: boolean })
    let recursive = if cc.length() > 1 {
        let opts = cc.arg(1);
        if opts.is_object() {
            opts.get_property_str(ctx, "recursive")?
                .to_bool()
                .unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        let r = if recursive {
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|e| e.to_string())
        } else {
            tokio::fs::create_dir(&path)
                .await
                .map_err(|e| e.to_string())
        };
        let _ = PollSender::from(tx).send_async(r).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// removeDir - remove a directory
//   removeDir(path)                        – remove an empty directory
//   removeDir(path, { recursive: true })   – recursively remove a directory tree
// ---------------------------------------------------------------------------
pub(super) fn js_fs_removeDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    // Parse optional options: removeDir(path, { recursive?: boolean })
    let opts = cc.arg(1);
    let recursive = if !opts.is_undefined() {
        if opts.is_object() {
            opts.get_property_str(ctx, "recursive")?.to_bool()?
        } else {
            return Err(ctx.throw_type_error("options must be an object"));
        }
    } else {
        false
    };

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        let r = if recursive {
            tokio::fs::remove_dir_all(&path)
                .await
                .map_err(|e| e.to_string())
        } else {
            tokio::fs::remove_dir(&path)
                .await
                .map_err(|e| e.to_string())
        };
        let _ = PollSender::from(tx).send_async(r).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// remove - delete a file or symbolic link (POSIX unlink semantics)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_remove(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

pub(super) fn js_fs_stat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) = Promise::create_with_channel::<Result<std::fs::Metadata, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let meta = payload.map_err(|e| cx.throw_internal_error(&e))?;
            make_stats(cx, &meta)
        },
    )?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(tokio::fs::metadata(&path).await.map_err(|e| e.to_string()))
            .await;
    });

    Ok(promise.into())
}

pub(super) const FS_COUNT_ALL: i32 = 0;
pub(super) const FS_COUNT_NON_EMPTY: i32 = 1;

/// Backend for `countLines` and `countNonEmptyLines` (registered with a
/// different magic const). Missing/unreadable files resolve to 0 rather than
/// rejecting the promise.
pub(super) fn js_count_lines(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(cx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());
    let non_empty = cc.magic() == FS_COUNT_NON_EMPTY;

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(cx, move |_, data| -> JSResult {
            let count = match data {
                Ok(text) => {
                    if non_empty {
                        text.lines().filter(|l| !l.trim().is_empty()).count()
                    } else {
                        text.lines().count()
                    }
                }
                Err(_) => 0,
            };
            Ok(JSValue::from(count as i32))
        })?;

    tokio::spawn(async move {
        let res = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string());
        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}
