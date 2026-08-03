// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::path::{Path, PathBuf};

use zuqe::{
    CallContext, JSResult, JSValue, PollSender, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::aike::JSFileHandle;

/// Helper: create a void promise for async operations returning Result<(), String>.
pub(super) fn make_void_promise(
    ctx: &mut ScopedContext,
) -> Result<
    (
        zuqe::builtins::promise::Promise,
        crossbeam::channel::Sender<Result<(), String>>,
    ),
    i32,
> {
    Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
        payload.map_err(|e| cx.throw_internal_error(&e))?;
        Ok(JSValue::UNDEFINED)
    })
}

/// Extract bytes from a JS value: supports ArrayBuffer & TypedArray (Uint8Array).
/// Takes &mut JSValue because payload_mut requires mutable access to ArrayBuffer.
fn get_bytes_from_arg(ctx: &mut ScopedContext, arg: &mut JSValue) -> Result<Vec<u8>, i32> {
    // Try ArrayBuffer
    if let Some(mut ab) = arg.as_object_ArrayBuffer() {
        let buf = ab.payload_mut();
        let mut bytes = Vec::new();
        buf.with_bytes_mut(|b| {
            bytes = b.to_vec();
        });
        return Ok(bytes);
    }

    // Try TypedArray: access .buffer property to get underlying ArrayBuffer
    if arg.as_object_TypedArray().is_some() {
        let buf_val = arg.get_property_str(ctx, "buffer")?;
        if let Some(mut ab) = buf_val.as_object_ArrayBuffer() {
            let buf = ab.payload_mut();
            let mut bytes = Vec::new();
            buf.with_bytes_mut(|b| {
                bytes = b.to_vec();
            });
            return Ok(bytes);
        }
    }

    Err(ctx.throw_type_error("expect Uint8Array or ArrayBuffer"))
}

// ---------------------------------------------------------------------------
// readText - read file content as UTF-8 string
// ---------------------------------------------------------------------------
pub(super) fn js_fs_readText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(ctx, |cx, payload| -> JSResult {
            let text = payload.map_err(|e| cx.throw_internal_error(&e))?;
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::make_string)
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
pub(super) fn js_fs_readJSON(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) = Promise::create_with_channel::<Result<serde_json::Value, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;
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
pub(super) fn js_fs_readBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| cx.throw_internal_error(&e))?;
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
pub(super) fn js_fs_writeText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());
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
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        let ret = unsafe {
            MoveFileExW(
                tmp_w.as_ptr(),
                dst_w.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ret != 0 {
            Ok(())
        } else {
            // Must call last_os_error() on THIS thread
            // (spawn_blocking thread), because GetLastError()
            // is thread-local. After .await we'd be on a
            // different tokio worker thread and the error
            // would be lost.
            Err(std::io::Error::last_os_error())
        }
    })
    .await
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("rename: {e}")))
}

// ---------------------------------------------------------------------------
// writeJSON - write JSON data to file
// ---------------------------------------------------------------------------
pub(super) fn js_fs_writeJSON(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());
    let val = cc
        .arg(1)
        .to_serde_json(ctx)
        .map_err(|_| ctx.throw_type_error("failed to serialize value to JSON"))?;

    let space = if cc.length() > 2 && !cc.arg(2).is_undefined() {
        Some(cc.arg(2).to_number(ctx)?.to_i32() as usize)
    } else {
        None
    };

    let json_str = if let Some(_n) = space {
        serde_json::to_string_pretty(&val).map_err(|e| ctx.throw_internal_error(&format!("{e}")))?
    } else {
        serde_json::to_string(&val).map_err(|e| ctx.throw_internal_error(&format!("{e}")))?
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
pub(super) fn js_fs_writeBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());
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
pub(super) fn js_fs_appendText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());
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
pub(super) fn js_fs_open(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

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
            let file = payload.map_err(|e| cx.throw_internal_error(&e))?;
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
pub(super) fn js_fs_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = PathBuf::from(v.content().encode_utf8(false).to_string());

    // string data
    if let Some(s) = cc.arg(1).as_string(true) {
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
        Err(_) => Err(ctx.throw_type_error("unsupported data type for write")),
    }
}
