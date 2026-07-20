// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    path::{Component, Path, PathBuf},
};

use zuqe::{
    Array, CallContext, FieldDef, JSResult, JSValue, NativeObject, PollSender, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::{
    aike::{JSFileHandle, make_stats},
    helpers::canonicalize_path,
};

thread_local! {
    /// Sandbox restriction: limits all fs access to this directory.
    /// None means no sandbox restriction.
    static SANDBOX_PATH: RefCell<Option<PathBuf>> = RefCell::new({
        let p = std::env::var("AIKE_SANDBOX_PATH").unwrap_or_default();
        if p.is_empty() {
            None
        } else {
            canonicalize_path(std::path::PathBuf::from(p))
                .ok()
                .and_then(|p| if p.is_dir() { Some(p) } else { None })
        }
    });
}

/// Normalize a path by resolving . and .. components without following symlinks.
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut rooted = false;

    for component in path.components() {
        match component {
            Component::Prefix(p) => {
                result.push(p.as_os_str());
            }
            Component::RootDir => {
                // Only set root once. On Windows, Prefix + RootDir forms an
                // absolute path (e.g. C:\), and we must not overwrite Prefix.
                if !rooted {
                    result.push(component.as_os_str());
                    rooted = true;
                }
            }
            Component::Normal(name) => result.push(name),
            Component::ParentDir => {
                // PathBuf::pop() guards against popping beyond root on all
                // platforms, so we only guard against popping an empty buf.
                if !result.as_os_str().is_empty() {
                    result.pop();
                }
            }
            Component::CurDir => {}
        }
    }
    result
}

/// Convert to a normalized absolute path. Relative paths are resolved against
/// the current working directory. Does not follow symlinks.
fn absolute_path(path: &Path) -> PathBuf {
    let abs = if path.has_root() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    normalize_path(&abs)
}

fn set_sandbox(path: Option<&str>) -> Result<(), String> {
    if path.is_none() {
        SANDBOX_PATH.with(|c| {
            *c.borrow_mut() = None;
        });
        return Ok(());
    }

    let abs_path = canonicalize_path(Path::new(path.unwrap())).map_err(|e| e.to_string())?;

    if !abs_path.is_dir() {
        return Err("sandbox path must be a directory".to_string());
    }
    SANDBOX_PATH.with(|s| {
        *s.borrow_mut() = Some(abs_path);
    });

    Ok(())
}

/// Check if a resolved path is within the sandbox boundary.
fn is_inside_sandbox(resolved_path: &Path) -> bool {
    let Some(sandbox) = SANDBOX_PATH.with(|c| c.borrow().clone()) else {
        return true;
    };
    resolved_path.starts_with(&sandbox)
}

/// Resolve a user-provided path to a safe filesystem path by applying sandbox restriction.
fn resolve_safe_path(user_path: &str) -> Result<PathBuf, String> {
    let user_path = Path::new(user_path);
    let abs_path = absolute_path(user_path);

    // Sandbox check: after chroot resolution, verify sandbox boundary
    if is_inside_sandbox(&abs_path) {
        Ok(abs_path)
    } else {
        Err(format!(
            "forbidden: path outside sandbox: {} ({})",
            user_path.display(),
            abs_path.display()
        ))
    }
}

/// Helper: create a void promise for async operations returning Result<(), String>.
fn make_void_promise(
    ctx: &mut ScopedContext,
) -> Result<
    (
        zuqe::builtins::promise::Promise,
        crossbeam::channel::Sender<Result<(), String>>,
    ),
    i32,
> {
    Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
        payload.map_err(|e| {
            cx.throw_internal_error(&e);
            -1
        })?;
        Ok(JSValue::UNDEFINED)
    })
}

/// Extract bytes from a JS value: supports ArrayBuffer & TypedArray (Uint8Array).
/// Takes &mut JSValue because payload_mut requires mutable access to ArrayBuffer.
fn get_bytes_from_arg(ctx: &mut ScopedContext, arg: &mut JSValue) -> Result<Vec<u8>, i32> {
    // Try ArrayBuffer
    if let Some(ab) = arg.as_object_ArrayBuffer_mut() {
        let buf = ab.payload_mut();
        let mut bytes = Vec::new();
        buf.with_bytes_mut(|b| {
            bytes = b.to_vec();
        });
        return Ok(bytes);
    }

    // Try TypedArray: access .buffer property to get underlying ArrayBuffer
    if arg.as_object_TypedArray().is_some() {
        let mut buf_val = arg.get_property_str(ctx, "buffer")?;
        if let Some(ab) = buf_val.as_object_ArrayBuffer_mut() {
            let buf = ab.payload_mut();
            let mut bytes = Vec::new();
            buf.with_bytes_mut(|b| {
                bytes = b.to_vec();
            });
            return Ok(bytes);
        }
    }

    ctx.throw_type_error("expect Uint8Array or ArrayBuffer");
    Err(-1)
}

const BOUNDARY_SANDBOX: i32 = 0;

fn js_fs_get_restrict(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = match cc.magic() {
        BOUNDARY_SANDBOX => SANDBOX_PATH.with(|c| c.borrow().clone()),
        _ => unreachable!(),
    };
    match path {
        Some(p) => {
            let s = p.to_string_lossy();
            ctx.create_string_from_utf8(s.as_bytes().to_vec())
                .map(JSValue::from)
        }
        None => Ok(JSValue::NULL),
    }
}

fn js_fs_set_restrict(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let arg = cc.arg(0);
    let path = if arg.is_undefined_or_null() {
        None
    } else {
        let v = arg.to_string(ctx)?;
        Some(v.content().encode_utf8(false).to_string())
    };

    match cc.magic() {
        BOUNDARY_SANDBOX => set_sandbox(path.as_deref()),
        _ => unreachable!(),
    }
    .map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    Ok(JSValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// readText - read file content as UTF-8 string
// ---------------------------------------------------------------------------
fn js_fs_readText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(ctx, |cx, payload| -> JSResult {
            let text = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// readJSON - read and parse JSON file
// ---------------------------------------------------------------------------
fn js_fs_readJSON(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    let (promise, tx) = Promise::create_with_channel::<Result<serde_json::Value, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let val = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            JSValue::from_serde_json(cx, &val)
        },
    )?;

    tokio::spawn(async move {
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string());

        let result = match raw {
            Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };

        let _ = PollSender::from(tx).send_async(result).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// readBytes - read file as binary data (Uint8Array)
// ---------------------------------------------------------------------------
fn js_fs_readBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(tokio::fs::read(&path).await.map_err(|e| e.to_string()))
            .await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// writeText - write string to file
// ---------------------------------------------------------------------------
fn js_fs_writeText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false)).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    let data = cc.arg(1).to_string(ctx)?;
    let data_str = data.content().encode_utf8(false).to_string();

    let (promise, tx) = make_void_promise(ctx)?;
    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                write_file_atomic(&path, data_str.as_bytes())
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

// Write `data` to `path` atomically: write to a sibling temp file then
// atomically replace the target. A crash mid-write leaves the original file
// intact (or a stale `.tmp` that the next write replaces), so readers never
// see a torn file. The actual replace is platform-specific (see atomic_replace).
// Exposed as `pub(crate)` so other modules (e.g. jsonl) reuse the same atomic
// write instead of rolling their own non-atomic truncate.
pub(crate) async fn write_file_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    });
    tokio::fs::write(&tmp, data).await?;
    atomic_replace(&tmp, path).await
}

// Atomically replace `dst` with `src` (both on the same filesystem).
#[cfg(not(windows))]
async fn atomic_replace(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    // POSIX rename() atomically overwrites an existing destination.
    tokio::fs::rename(tmp, dst).await
}

#[cfg(windows)]
async fn atomic_replace(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows_sys::core::PCWSTR;

    // MoveFileExW with MOVEFILE_REPLACE_EXISTING is the atomic replace on
    // Windows (std::fs::rename refuses to replace an existing file). The
    // WRITE_THROUGH flag makes the replace durable before it returns.
    let to_wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let tmp_w = to_wide(tmp);
    let dst_w = to_wide(dst);
    let ok = tokio::task::spawn_blocking(move || unsafe {
        MoveFileExW(
            PCWSTR(tmp_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ) != 0
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

// ---------------------------------------------------------------------------
// writeJSON - write JSON data to file
// ---------------------------------------------------------------------------
fn js_fs_writeJSON(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    let val = cc.arg(1).to_serde_json(ctx).map_err(|_| {
        ctx.throw_type_error("failed to serialize value to JSON");
        -1
    })?;

    let space = if cc.length() > 2 && !cc.arg(2).is_undefined() {
        Some(cc.arg(2).to_number(ctx)?.to_i32() as usize)
    } else {
        None
    };

    let json_str = if let Some(_n) = space {
        serde_json::to_string_pretty(&val).map_err(|e| {
            ctx.throw_internal_error(&format!("{e}"));
            -1
        })?
    } else {
        serde_json::to_string(&val).map_err(|e| {
            ctx.throw_internal_error(&format!("{e}"));
            -1
        })?
    };

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        let res = write_file_atomic(&path, json_str.as_bytes()).await;
        let _ = PollSender::from(tx)
            .send_async(res.map_err(|e| e.to_string()))
            .await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// writeBytes - write binary data (Uint8Array) to file
// ---------------------------------------------------------------------------
fn js_fs_writeBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    let bytes = get_bytes_from_arg(ctx, &mut cc.arg(1))?;

    let (promise, tx) = make_void_promise(ctx)?;
    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                write_file_atomic(&path, &bytes)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// appendText - append text content to file
// ---------------------------------------------------------------------------
fn js_fs_appendText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    let data = cc.arg(1).to_string(ctx)?;
    let data_str = data.content().encode_utf8(false).to_string();

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;

        let res: Result<(), String> = match tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await
        {
            Ok(mut f) => f
                .write_all(data_str.as_bytes())
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };

        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// open - open a file and return a real FileHandle with fd
// ---------------------------------------------------------------------------
fn js_fs_open(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    // Parse flags: default 'r'
    let flags = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        let f = cc.arg(1).to_string(ctx)?;
        f.content().encode_utf8(false).to_string()
    } else {
        "r".to_string()
    };

    let (promise, tx) = Promise::create_with_channel::<Result<tokio::fs::File, String>>(
        ctx,
        move |cx, payload| -> JSResult {
            let file = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            JSFileHandle::new_instance_file(cx, JSValue::UNDEFINED, file).map(JSValue::from)
        },
    )?;

    tokio::spawn(async move {
        let mut opts = tokio::fs::OpenOptions::new();
        match flags.as_str() {
            "r" | "rb" | "rrr" | "" => {
                opts.read(true);
            }
            "r+" | "r+b" | "rb+" => {
                opts.read(true).write(true);
            }
            "w" | "wb" => {
                opts.write(true).create(true).truncate(true);
            }
            "w+" | "w+b" | "wb+" => {
                opts.read(true).write(true).create(true).truncate(true);
            }
            "a" | "ab" => {
                opts.append(true).create(true);
            }
            "a+" | "a+b" | "ab+" => {
                opts.read(true).append(true).create(true);
            }
            "rw" => {
                opts.read(true).write(true).create(true);
            }
            _ => {
                opts.read(true);
            }
        }
        let result = opts.open(&path).await.map_err(|e| e.to_string());
        let _ = PollSender::from(tx).send_async(result).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// write - multi-format unified write
// ---------------------------------------------------------------------------
fn js_fs_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    // string data
    if let Some(s) = cc.arg(1).as_string() {
        let content = s.content().encode_utf8(false).to_string();
        let (promise, tx) = make_void_promise(ctx)?;

        tokio::spawn(async move {
            let _ = PollSender::from(tx)
                .send_async(
                    write_file_atomic(&path, content.as_bytes())
                        .await
                        .map_err(|e| e.to_string()),
                )
                .await;
        });

        return Ok(promise.into());
    }

    // Uint8Array or ArrayBuffer
    match get_bytes_from_arg(ctx, &mut cc.arg(1)) {
        Ok(bytes) => {
            let (promise, tx) = make_void_promise(ctx)?;
            tokio::spawn(async move {
                let _ = PollSender::from(tx)
                    .send_async(
                        write_file_atomic(&path, &bytes)
                            .await
                            .map_err(|e| e.to_string()),
                    )
                    .await;
            });
            return Ok(promise.into());
        }
        Err(_) => {
            ctx.throw_type_error("unsupported data type for write");
            Err(-1)
        }
    }
}

// ---------------------------------------------------------------------------
// ensureDir - ensure directory exists (idempotent, default recursive)
// ---------------------------------------------------------------------------
fn js_fs_ensureDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(v.content().encode_utf8(false).as_ref()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    // Parse optional options object: ensureDir(path, { recursive?: boolean })
    let opts = cc.arg(1);
    let recursive = if !opts.is_undefined() {
        if opts.is_object() {
            opts.get_property_str(ctx, "recursive")?.to_bool()?
        } else {
            ctx.throw_type_error("options must be an object");
            return Err(-1);
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
    obj.define_property_value_str(ctx, "name", name_s.into(), zuqe::JS_PROP_ENUMERABLE)?;

    obj.define_property_value_str(
        ctx,
        "isFile",
        JSValue::Bool(d.is_file),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isDirectory",
        JSValue::Bool(d.is_dir),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isSymbolicLink",
        JSValue::Bool(d.is_symlink),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    let size = JSValue::from(d.size as i64);
    obj.define_property_value_str(ctx, "size", size, zuqe::JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

// ---------------------------------------------------------------------------
// readDir - read directory contents
// ---------------------------------------------------------------------------
fn js_fs_readDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    let (promise, tx) = Promise::create_with_channel::<Result<Vec<DirentData>, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let entries = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

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
fn js_fs_exists(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<bool, String>>(ctx, |cx, payload| -> JSResult {
            let exists = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::Bool(exists))
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
fn js_fs_makeDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

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
fn js_fs_removeDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    // Parse optional options: removeDir(path, { recursive?: boolean })
    let opts = cc.arg(1);
    let recursive = if !opts.is_undefined() {
        if opts.is_object() {
            opts.get_property_str(ctx, "recursive")?.to_bool()?
        } else {
            ctx.throw_type_error("options must be an object");
            return Err(-1);
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
fn js_fs_remove(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

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

fn js_fs_stat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    let (promise, tx) = Promise::create_with_channel::<Result<std::fs::Metadata, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let meta = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
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

const FS_COUNT_ALL: i32 = 0;
const FS_COUNT_NON_EMPTY: i32 = 1;

/// Backend for `countLines` and `countNonEmptyLines` (registered with a
/// different magic const). Missing/unreadable files resolve to 0 rather than
/// rejecting the promise.
fn js_count_lines(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(cx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        cx.throw_internal_error(&e);
        -1
    })?;
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

const MOD_FS_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        JSFileHandle::CLASS_NAME,
        JSFileHandle::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    ),
    FieldDef::FUNC("getSandbox", 0, js_fs_get_restrict, BOUNDARY_SANDBOX as _),
    FieldDef::FUNC("setSandbox", 1, js_fs_set_restrict, BOUNDARY_SANDBOX as _),
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
