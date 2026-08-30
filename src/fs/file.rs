// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    future::Future,
    io::SeekFrom,
    path::{Path, PathBuf},
};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use zuqe::{
    CallContext, JSResult, JSValue, PollSender, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::aike::JSFileHandle;

/// Extract a filesystem path from JS argument `0`.
pub(super) fn get_path_arg(ctx: &mut ScopedContext, cc: &CallContext) -> Result<PathBuf, i32> {
    let v = cc.arg(0).to_string(ctx)?;
    Ok(PathBuf::from(v.content().encode_utf8(false).to_string()))
}

/// Run an async filesystem operation in a background task and resolve a Promise
/// with the converted result; `Err` payloads reject the Promise.
pub(super) fn spawn_fs_op<T, F, Fut>(
    ctx: &mut ScopedContext,
    fut: Fut,
    convert: F,
) -> Result<Promise, i32>
where
    T: Send + Unpin + 'static,
    F: FnOnce(&mut ScopedContext, T) -> Result<JSValue, i32> + 'static,
    Fut: Future<Output = Result<T, String>> + Send + 'static,
{
    let (promise, tx) =
        Promise::create_with_channel::<Result<T, String>>(ctx, |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;
            convert(cx, val)
        })?;
    tokio::spawn(async move {
        let _ = tx.send_async(fut.await).await;
    });
    Ok(promise)
}

/// Like [`spawn_fs_op`] but for operations that resolve with no value.
pub(super) fn spawn_fs_void<Fut>(ctx: &mut ScopedContext, fut: Fut) -> Result<Promise, i32>
where
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    spawn_fs_op(ctx, fut, |_cx, ()| Ok(JSValue::UNDEFINED))
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
// read - read file content; magic const picks text (UTF-8 string) or bytes
// ---------------------------------------------------------------------------
pub(super) const FS_READ_TEXT: i32 = 0;
pub(super) const FS_READ_BYTES: i32 = 1;

pub(super) fn js_fs_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let as_text = cc.magic() == FS_READ_TEXT;

    // Optional { offset?: number, length?: number }: start reading at `offset`
    // and read at most `length` bytes (fewer if fewer remain). Negative values
    // clamp to 0; length 0 returns empty.
    let mut offset: u64 = 0;
    let mut limit: Option<u64> = None;
    if cc.length() > 1 && cc.arg(1).is_object() {
        let opts = cc.arg(1);
        let off = opts.get_property_str(ctx, "offset")?;
        if !off.is_undefined() {
            offset = off.to_number(ctx)?.to_i64_saturating().max(0) as u64;
        }
        let len = opts.get_property_str(ctx, "length")?;
        if !len.is_undefined() {
            limit = Some(len.to_number(ctx)?.to_i64_saturating().max(0) as u64);
        }
    }

    spawn_fs_op(
        ctx,
        async move {
            let mut file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| e.to_string())?;
            if offset > 0 {
                file.seek(SeekFrom::Start(offset))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let mut bytes = Vec::new();
            match limit {
                Some(n) => file
                    .take(n)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?,
                None => file
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| e.to_string())?,
            };
            Ok(bytes)
        },
        move |cx, bytes| {
            if as_text {
                // A partial range may split a multi-byte UTF-8 char; refuse to
                // hand back invalid text.
                let s = String::from_utf8(bytes)
                    .map_err(|_| cx.throw_type_error("read range is not valid UTF-8"))?;
                cx.create_string_from_utf8(s.into_bytes())
                    .map(JSValue::make_string)
            } else {
                TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from)
            }
        },
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// readJSON - read and parse JSON file
// ---------------------------------------------------------------------------
pub(super) fn js_fs_readJSON(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            let raw = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&raw).map_err(|e| e.to_string())
        },
        |cx, val: serde_json::Value| JSValue::from_serde_json(cx, &val),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// write - write string or bytes to file; magic const picks the data format
// ---------------------------------------------------------------------------
pub(super) const FS_WRITE_TEXT: i32 = 0;
pub(super) const FS_WRITE_BYTES: i32 = 1;

pub(super) fn js_fs_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let data = if cc.magic() == FS_WRITE_TEXT {
        let s = cc.arg(1).to_string(ctx)?;
        s.content().encode_utf8(false).to_string().into_bytes()
    } else {
        get_bytes_from_arg(ctx, &mut cc.arg(1))?
    };
    spawn_fs_void(ctx, async move {
        write_file_atomic(&path, &data)
            .await
            .map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// Write `data` to `path` atomically: write to a sibling temp file then
// atomically replace the target. A crash mid-write leaves the original file
// intact (or a stale `.tmp` that the next write replaces), so readers never
// see a torn file. The actual replace is platform-specific (see atomic_replace).
// Exposed as `pub(crate)` so other modules (e.g. jsonl) reuse the same atomic
// write instead of rolling their own non-atomic truncate.
pub(crate) async fn write_file_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = temp_path_for(path);
    tokio::fs::write(&tmp, data).await?;
    atomic_replace(&tmp, path).await
}

/// Sibling temp file path used for atomic writes/replacements.
fn temp_path_for(path: &Path) -> PathBuf {
    path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    })
}

/// A guard around a temp-file write: content goes into `<name>.tmp`, and
/// `commit()` atomically renames it over the final path. Dropping the guard
/// without committing removes the partial temp file, so an aborted write
/// (e.g. `Aike.download`) leaves nothing behind.
pub(crate) struct TempFileGuard {
    tmp: PathBuf,
    final_path: PathBuf,
    committed: bool,
}

impl TempFileGuard {
    /// Create a guard for `final_path`; the temp file is a sibling
    /// (`<final_name>.tmp`).
    pub(crate) fn new(final_path: PathBuf) -> Self {
        let tmp = temp_path_for(&final_path);
        Self {
            tmp,
            final_path,
            committed: false,
        }
    }

    /// The temp file path to write the content into.
    pub(crate) fn temp_path(&self) -> &Path {
        &self.tmp
    }

    /// Atomically rename the temp file over the final path.
    pub(crate) async fn commit(&mut self) -> std::io::Result<()> {
        atomic_replace(&self.tmp, &self.final_path).await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
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
    let path = get_path_arg(ctx, &cc)?;
    let val = cc
        .arg(1)
        .to_serde_json(ctx)
        .map_err(|_| ctx.throw_type_error("failed to serialize value to JSON"))?;

    let pretty = cc.length() > 2 && !cc.arg(2).is_undefined();
    let json_str = if pretty {
        serde_json::to_string_pretty(&val).map_err(|e| ctx.throw_internal_error(&e.to_string()))?
    } else {
        serde_json::to_string(&val).map_err(|e| ctx.throw_internal_error(&e.to_string()))?
    };

    spawn_fs_void(ctx, async move {
        write_file_atomic(&path, json_str.as_bytes())
            .await
            .map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// sha256 - hash a file's contents (lowercase hex digest)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_sha256(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            let data = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
            use sha2::Digest;
            let digest = sha2::Sha256::digest(&data);
            Ok(digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>())
        },
        |cx, s| cx.create_string_from_str(&s).map(JSValue::make_string),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// realpath - canonicalize a path, resolving all symlinks
// ---------------------------------------------------------------------------
pub(super) fn js_fs_realpath(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            tokio::fs::canonicalize(&path)
                .await
                .map(|p| p.to_string_lossy().to_string())
                .map_err(|e| e.to_string())
        },
        |cx, s| cx.create_string_from_str(&s).map(JSValue::make_string),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// appendText - append text content to file
// ---------------------------------------------------------------------------
pub(super) fn js_fs_appendText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let data = cc.arg(1).to_string(ctx)?;
    let data = data.content().encode_utf8(false).to_string();
    spawn_fs_void(ctx, async move {
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await
            .map_err(|e| e.to_string())?;
        f.write_all(data.as_bytes())
            .await
            .map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// replaceText - unified replace (unique / replaceAll), selected via magic const.
// ---------------------------------------------------------------------------
pub(super) const FS_REPLACE_UNIQUE: i32 = 0;
pub(super) const FS_REPLACE_ALL: i32 = 1;

pub(super) fn js_fs_replaceText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let replace_all = cc.magic() == FS_REPLACE_ALL;

    let path = get_path_arg(ctx, &cc)?;
    let old_text = cc.arg(1).to_string(ctx)?;
    let old_text = old_text.content().encode_utf8(false).to_string();
    let replace_with = cc.arg(2).to_string(ctx)?;
    let replace_with = replace_with.content().encode_utf8(false).to_string();

    spawn_fs_op(
        ctx,
        async move {
            if old_text.is_empty() || old_text == replace_with {
                return Ok(0);
            }

            // ponytail: whole-file in memory (O(file)); stream if huge files matter
            let Ok(data) = tokio::fs::read(&path).await else {
                return Ok(0); // unreadable → no replacements, as before
            };
            let needle = old_text.as_bytes();
            let replacement = replace_with.as_bytes();

            // Unique mode: exactly one (overlapping-counted) occurrence; any
            // second match means no-op. ReplaceAll: left-to-right non-overlapping.
            let mut out = Vec::with_capacity(data.len());
            let mut count: i32 = 0;
            let mut last = 0usize;
            let mut i = 0usize;
            while i + needle.len() <= data.len() {
                if &data[i..i + needle.len()] == needle {
                    out.extend_from_slice(&data[last..i]);
                    out.extend_from_slice(replacement);
                    let start = i;
                    i += needle.len();
                    last = i;
                    count += 1;
                    if !replace_all {
                        // any further occurrence — even one overlapping this
                        // match — makes it non-unique (old semantics)
                        let mut j = start + 1;
                        while j + needle.len() <= data.len() {
                            if &data[j..j + needle.len()] == needle {
                                return Ok(0);
                            }
                            j += 1;
                        }
                        break;
                    }
                } else {
                    i += 1;
                }
            }

            if count == 0 {
                return Ok(0);
            }
            out.extend_from_slice(&data[last..]);

            match write_file_atomic(&path, &out).await {
                Ok(()) => Ok(count),
                Err(_) => {
                    let _ = tokio::fs::remove_file(temp_path_for(&path)).await;
                    Ok(0)
                }
            }
        },
        |_cx, count| Ok(JSValue::from(count)),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// open - open a file and return a real FileHandle with fd
// ---------------------------------------------------------------------------
pub(super) fn js_fs_open(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;

    // Parse flags: default 'r'
    let flags = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        let f = cc.arg(1).to_string(ctx)?;
        f.content().encode_utf8(false).to_string()
    } else {
        "r".to_string()
    };

    spawn_fs_op(
        ctx,
        async move {
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
            opts.open(&path).await.map_err(|e| e.to_string())
        },
        |cx, file| JSFileHandle::new_instance_file(cx, JSValue::UNDEFINED, file).map(JSValue::from),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// write - multi-format unified write (auto-detects string vs bytes)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_write_auto(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let data = if let Some(s) = cc.arg(1).as_string(true) {
        s.content().encode_utf8(false).to_string().into_bytes()
    } else {
        match get_bytes_from_arg(ctx, &mut cc.arg(1)) {
            Ok(bytes) => bytes,
            Err(_) => return Err(ctx.throw_type_error("unsupported data type for write")),
        }
    };
    spawn_fs_void(ctx, async move {
        write_file_atomic(&path, &data)
            .await
            .map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}
