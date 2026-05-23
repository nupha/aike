// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

pub(crate) mod class_file_handle;

use std::{
    cell::RefCell,
    path::{Component, Path, PathBuf},
};

use zuqe::{
    Array, CallContext, FieldDef, JSResult, JSValue, PollSender, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::{
    AIKE_SANDBOX_PATH,
    fs::class_file_handle::{JSFileHandle, make_stats},
};

thread_local! {
    /// Chroot support
    static CHROOT_PATH: RefCell<Option<PathBuf>> = RefCell::new(None);

    /// Sandbox restriction: limits all fs access to this directory.
    /// None means no sandbox restriction.
    static SANDBOX_PATH: RefCell<Option<PathBuf>> = RefCell::new(if AIKE_SANDBOX_PATH.is_empty() {
        None
    } else {
        std::fs::canonicalize(std::path::PathBuf::from(AIKE_SANDBOX_PATH))
        .ok()
        .and_then(|p| if p.is_dir() { Some(p) } else { None })
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

fn set_chroot(path: Option<&str>) -> Result<(), String> {
    if path.is_none() {
        CHROOT_PATH.with(|c| {
            *c.borrow_mut() = None;
        });
        return Ok(());
    }

    let abs_path = std::fs::canonicalize(Path::new(path.unwrap())).map_err(|e| e.to_string())?;

    if !abs_path.is_dir() {
        return Err("chroot path must be a directory".to_string());
    }
    CHROOT_PATH.with(|s| {
        *s.borrow_mut() = Some(abs_path);
    });

    Ok(())
}

fn set_sandbox(path: Option<&str>) -> Result<(), String> {
    if path.is_none() {
        SANDBOX_PATH.with(|c| {
            *c.borrow_mut() = None;
        });
        return Ok(());
    }

    let abs_path = std::fs::canonicalize(Path::new(path.unwrap())).map_err(|e| e.to_string())?;

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

/// Resolve a user-provided path to a safe filesystem path by applying
/// chroot isolation, sandbox restriction, and symlink-escape checks.
///
/// Processing order:
/// 1. If CHROOT is set, prepend it to the user path and normalize.
/// 2. Lexical boundary check: ensure the path stays within chroot.
/// 3. Sandbox boundary check: ensure the path stays within SANDBOX_PATH.
/// 4. Symlink resolution: canonicalize the path if it exists, then
///    re-verify the sandbox boundary on the real path
///    (prevents symlink-escape attacks).
///
/// Returns the resolved path as-is if neither CHROOT nor SANDBOX is set.
/// When only SANDBOX is set, the path is resolved to absolute for boundary
/// comparison but returned in its original form.
/// Returns an error if any boundary check fails.
fn resolve_safe_path(user_path: &str) -> Result<PathBuf, String> {
    let Some(chroot) = CHROOT_PATH.with(|c| c.borrow().clone()) else {
        let p = PathBuf::from(user_path);
        let has_sandbox = SANDBOX_PATH.with(|c| c.borrow().is_some());

        if has_sandbox {
            // Resolve to absolute for proper sandbox boundary comparison.
            // canonicalize handles existing paths correctly (follows CWD).
            // For writes to files that don't exist yet, canonicalize fails —
            // we fall back to CWD + normalize so relative paths can still
            // be compared against the absolute sandbox boundary.
            let abs = match std::fs::canonicalize(&p) {
                Ok(real) => real,
                Err(_) => {
                    // File doesn't exist yet (e.g. writeText to a new file).
                    // Canonicalize requires an existing path, so we manually
                    // resolve relative to CWD to get an absolute path for
                    // sandbox comparison.
                    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
                    normalize_path(&cwd.join(&p))
                }
            };

            if !is_inside_sandbox(&abs) {
                return Err(format!("forbidden: path outside sandbox: {}", user_path));
            }
        }

        return Ok(p);
    };

    // Build raw path: absolute user paths are relative to chroot.
    // Works cross-platform:
    //   Unix:   "/foo"  → "foo"
    //   Win:    "C:\foo" → "foo"  (strip Prefix + RootDir)
    let mut raw = chroot;
    let user_path = Path::new(user_path);
    if user_path.has_root() {
        // Strip leading prefix/root to get the relative portion.
        // Covers all platforms:
        //   Unix:   "/foo"                → "foo"   (has_root=true)
        //   Win:    "C:\foo"  "D:\bar"     → "foo","bar" (has_root=true)
        //   Win:    "\foo" (root-relative)  → "foo"   (has_root=true)
        //   Win:    "/foo" (root-relative)  → "foo"   (has_root=true)
        //   Either: "foo" (relative)        → else    (has_root=false)
        let relative = user_path
            .components()
            .skip_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
            .map(|c| c.as_os_str())
            .collect::<PathBuf>();
        raw.push(relative);
    } else {
        raw.push(user_path);
    }

    let normalized = normalize_path(&raw);

    // Lexical check: must stay within chroot (fast path, no I/O)
    let allowed = CHROOT_PATH.with(|c| {
        c.borrow()
            .as_ref()
            .map_or(false, |s| normalized.starts_with(s))
    });
    if !allowed {
        return Err("forbidden: path outside chroot".to_string());
    }

    // Sandbox check: after chroot resolution, verify sandbox boundary
    if !is_inside_sandbox(&normalized) {
        return Err(format!(
            "forbidden: path outside sandbox: {}",
            user_path.display()
        ));
    }

    // Resolve symlinks: canonicalize the path if the file exists.
    // This prevents symlink-escape attacks where a symlink inside
    // the chroot points to a path outside (e.g. /etc/shadow).
    // Writes to non-existent paths fall back to the lexical check above,
    // which is safe because no symlink can be embedded in a non-existent path.
    match std::fs::canonicalize(&normalized) {
        Ok(real) => {
            let fs_allowed =
                CHROOT_PATH.with(|c| c.borrow().as_ref().map_or(false, |s| real.starts_with(s)));
            if fs_allowed {
                // Sandbox check on canonicalized path
                if !is_inside_sandbox(&real) {
                    return Err(format!(
                        "forbidden: path outside sandbox: {}",
                        user_path.display()
                    ));
                }
                // Use real path so the kernel sees the resolved target
                Ok(real)
            } else {
                Err(format!(
                    "forbidden: symlink escape: {} -> {} {:?}",
                    user_path.display(),
                    real.display(),
                    CHROOT_PATH.with(|c| c.borrow().as_ref().map(|s| s.to_owned()))
                ))
            }
        }
        Err(_) => {
            // Path doesn't exist yet (write, mkdir, etc.) —
            // lexical path is the only option and safe
            Ok(normalized)
        }
    }
}

/// Strip chroot prefix from a resolved path for external display.
/// Returns the path as-is if no chroot is set.
pub(crate) fn strip_chroot(full_path: &str) -> String {
    let Some(chroot) = CHROOT_PATH.with(|c| c.borrow().clone()) else {
        return full_path.to_string();
    };

    let normalized = normalize_path(Path::new(full_path));

    if let Ok(stripped) = normalized.strip_prefix(&chroot) {
        if stripped.as_os_str().is_empty() {
            "/".to_string()
        } else {
            // Normalize to forward slashes for cross-platform consistency:
            // JS layer passes Unix-style paths, strip output should match.
            stripped.to_string_lossy().replace('\\', "/")
        }
    } else {
        full_path.to_string()
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
const BOUNDARY_CHROOT: i32 = 1;

fn js_fs_get_boundary(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = match cc.magic() {
        BOUNDARY_SANDBOX => SANDBOX_PATH.with(|c| c.borrow().clone()),
        BOUNDARY_CHROOT => CHROOT_PATH.with(|c| c.borrow().clone()),
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

fn js_fs_set_boundary(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let arg = cc.arg(0);
    let path = if arg.is_undefined_or_null() {
        None
    } else {
        let v = arg.to_string(ctx)?;
        Some(v.content().encode_utf8(false).to_string())
    };

    match cc.magic() {
        BOUNDARY_SANDBOX => set_sandbox(path.as_deref()),
        BOUNDARY_CHROOT => set_chroot(path.as_deref()),
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
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;
    let data = cc.arg(1).to_string(ctx)?;
    let data_str = data.content().encode_utf8(false).to_string();

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::write(&path, data_str.as_bytes())
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
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
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::write(&path, json_str.as_bytes())
                    .await
                    .map_err(|e| e.to_string()),
            )
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
                tokio::fs::write(&path, &bytes)
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
            JSFileHandle::new_instance(cx, JSValue::UNDEFINED, file).map(JSValue::Object)
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
                    tokio::fs::write(&path, content.as_bytes())
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
                        tokio::fs::write(&path, &bytes)
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
// ensureDir - ensure directory exists
// ---------------------------------------------------------------------------
fn js_fs_ensureDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = resolve_safe_path(&v.content().encode_utf8(false).to_string()).map_err(|e| {
        ctx.throw_internal_error(&e);
        -1
    })?;

    // Parse optional options object: ensureDir(path, { recursive?: boolean })
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

const MOD_FS_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("getSandbox", 0, js_fs_get_boundary, BOUNDARY_SANDBOX as _),
    FieldDef::FUNC("getChroot", 0, js_fs_get_boundary, BOUNDARY_CHROOT as _),
    FieldDef::FUNC("setSandbox", 1, js_fs_set_boundary, BOUNDARY_SANDBOX as _),
    FieldDef::FUNC("setChroot", 1, js_fs_set_boundary, BOUNDARY_CHROOT as _),
    FieldDef::FUNC("open", 2, js_fs_open, 0),
    FieldDef::FUNC("readText", 1, js_fs_readText, 0),
    FieldDef::FUNC("readJSON", 1, js_fs_readJSON, 0),
    FieldDef::FUNC("readBytes", 1, js_fs_readBytes, 0),
    FieldDef::FUNC("writeText", 2, js_fs_writeText, 0),
    FieldDef::FUNC("writeJSON", 2, js_fs_writeJSON, 0),
    FieldDef::FUNC("writeBytes", 2, js_fs_writeBytes, 0),
    FieldDef::FUNC("appendText", 2, js_fs_appendText, 0),
    FieldDef::FUNC("write", 2, js_fs_write, 0),
    FieldDef::FUNC("readDir", 1, js_fs_readDir, 0),
    FieldDef::FUNC("ensureDir", 2, js_fs_ensureDir, 0),
    FieldDef::FUNC("stat", 1, js_fs_stat, 0),
];

pub fn init_mod_fs(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("fs", MOD_FS_ENTRIES)?;
    Ok(())
}
