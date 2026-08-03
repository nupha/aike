// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{future::Future, path::{Path, PathBuf}};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        let _ = PollSender::from(tx).send_async(fut.await).await;
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
// readText - read file content as UTF-8 string
// ---------------------------------------------------------------------------
pub(super) fn js_fs_readText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| e.to_string())
        },
        |cx, text| {
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::make_string)
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
// readBytes - read file as binary data (Uint8Array)
// ---------------------------------------------------------------------------
pub(super) fn js_fs_readBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    spawn_fs_op(
        ctx,
        async move {
            tokio::fs::read(&path).await.map_err(|e| e.to_string())
        },
        |cx, bytes| TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from),
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// writeText - write string to file
// ---------------------------------------------------------------------------
pub(super) fn js_fs_writeText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let data = cc.arg(1).to_string(ctx)?;
    let data = data.content().encode_utf8(false).to_string();
    spawn_fs_void(ctx, async move {
        write_file_atomic(&path, data.as_bytes())
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
// writeBytes - write binary data (Uint8Array) to file
// ---------------------------------------------------------------------------
pub(super) fn js_fs_writeBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let bytes = get_bytes_from_arg(ctx, &mut cc.arg(1))?;
    spawn_fs_void(ctx, async move {
        write_file_atomic(&path, &bytes)
            .await
            .map_err(|e| e.to_string())
    })
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
        f.write_all(data.as_bytes()).await.map_err(|e| e.to_string())
    })
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// replaceTextUnique - replace text in file when oldText matches exactly once
// ---------------------------------------------------------------------------
pub(super) fn js_fs_replaceTextUnique(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = get_path_arg(ctx, &cc)?;
    let old_text = cc.arg(1).to_string(ctx)?;
    let old_text = old_text.content().encode_utf8(false).to_string();
    let replace_with = cc.arg(2).to_string(ctx)?;
    let replace_with = replace_with.content().encode_utf8(false).to_string();

    spawn_fs_op(
        ctx,
        async move {
            // Empty oldText or a no-op replacement never replaces.
            if old_text.is_empty() || old_text == replace_with {
                return Ok(false);
            }
            // Pass 1: stream-count occurrences so a huge file is never loaded
            // into memory (bounded to chunk size + needle length).
            let Ok(Some(at)) = find_unique_match(&path, old_text.as_bytes()).await else {
                return Ok(false);
            };
            // Pass 2: stream-copy to a temp file, splicing the replacement at
            // the single match offset, then atomically swap it in.
            let tmp = temp_path_for(&path);
            let res = async {
                copy_replace(&path, &tmp, at, old_text.len(), replace_with.as_bytes()).await?;
                atomic_replace(&tmp, &path).await
            }
            .await;
            if res.is_err() {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Ok(false);
            }
            Ok(true)
        },
        |_cx, ok| Ok(JSValue::from(ok)),
    )
    .map(JSValue::from)
}

// Chunk size for streaming the file; keeps peak memory bounded for large files.
const STREAM_CHUNK: usize = 1 << 20; // 1 MiB

/// Return the byte offset of `needle` in the file if it occurs exactly once
/// (counting overlapping matches), streaming in chunks. `None` means the match
/// is missing or not unique, or the file cannot be read.
async fn find_unique_match(path: &Path, needle: &[u8]) -> std::io::Result<Option<u64>> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut carry: Vec<u8> = Vec::new(); // tail of the previous window
    let mut base: u64 = 0; // absolute offset of carry[0]
    let mut count = 0u32;
    let mut first: Option<u64> = None;
    let mut buf = vec![0u8; STREAM_CHUNK];

    loop {
        let n = file.read(&mut buf).await?;
        let eof = n == 0;
        carry.extend_from_slice(&buf[..n]);
        let win = &carry[..];

        // Count every match fully contained in this window (starting anywhere in
        // it); matches straddling the previous boundary are caught because carry
        // keeps the last needle_len-1 bytes. Advance byte-by-byte so overlapping
        // matches are all seen (e.g. "aa" in "aaa" counts 2).
        if win.len() >= needle.len() {
            let last = win.len() - needle.len();
            let mut s = 0;
            while s <= last {
                if win[s..s + needle.len()] == *needle {
                    count += 1;
                    if count == 1 {
                        first = Some(base + s as u64);
                    } else {
                        return Ok(None); // not unique
                    }
                }
                s += 1;
            }
        }

        if eof {
            break;
        }
        // Keep the tail that could be a prefix of a match spanning the boundary.
        let keep = needle.len() - 1;
        let drop = win.len() - keep.min(win.len());
        base += drop as u64;
        carry.drain(..drop);
    }
    Ok(first)
}

/// Stream-copy `src` to `dst`, replacing `needle` (the unique match starting at
/// byte offset `at`) with `replacement`. Fails if the file no longer matches.
async fn copy_replace(
    src: &Path,
    dst: &Path,
    at: u64,
    needle_len: usize,
    replacement: &[u8],
) -> std::io::Result<()> {
    let mut r = tokio::fs::File::open(src).await?;
    let mut w = tokio::fs::File::create(dst).await?;
    let mut pos: u64 = 0; // input bytes consumed
    let mut skip: usize = 0; // needle bytes still to consume after the splice
    let mut spliced = false;
    let mut buf = vec![0u8; STREAM_CHUNK];

    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let mut chunk = &buf[..n];

        // Consume the needle bytes skipped right after the splice point.
        if skip > 0 {
            let k = skip.min(chunk.len());
            chunk = &chunk[k..];
            skip -= k;
            pos += k as u64;
            if skip == 0 {
                spliced = true;
            }
            if chunk.is_empty() {
                continue;
            }
        }

        if spliced || pos >= at + needle_len as u64 {
            // Everything from here on is past the replacement point.
            w.write_all(chunk).await?;
            pos += chunk.len() as u64;
        } else if pos + chunk.len() as u64 <= at {
            // Entire chunk lies before the match.
            w.write_all(chunk).await?;
            pos += chunk.len() as u64;
        } else {
            // Chunk straddles the match start.
            let cut = (at - pos) as usize;
            w.write_all(&chunk[..cut]).await?;
            w.write_all(replacement).await?;
            pos = at;
            let rest = &chunk[cut..];
            let k = needle_len.min(rest.len());
            skip = needle_len - k;
            let rest = &rest[k..];
            pos += k as u64;
            if skip == 0 {
                spliced = true;
            }
            if !rest.is_empty() {
                w.write_all(rest).await?;
                pos += rest.len() as u64;
            }
        }
    }

    if spliced && skip == 0 {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "file changed while replacing",
        ))
    }
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
        |cx, file| {
            JSFileHandle::new_instance_file(cx, JSValue::UNDEFINED, file).map(JSValue::from)
        },
    )
    .map(JSValue::from)
}

// ---------------------------------------------------------------------------
// write - multi-format unified write
// ---------------------------------------------------------------------------
pub(super) fn js_fs_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
