// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

use std::{
    future::Future,
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
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
// intact, so readers never see a torn file. The actual replace is
// platform-specific (see atomic_replace).
// Exposed as `pub(crate)` so other modules (e.g. jsonl) reuse the same atomic
// write instead of rolling their own non-atomic truncate.
//
// The temp name is unique per write (pid + monotonic counter), aligned with
// npm's write-file-atomic (`filename + "." + hash(pid, invocations)`): two
// concurrent writers each draft into their own file, so neither rename can
// eat the other's temp (the old fixed `x.txt.tmp` name made the loser fail
// with ENOENT while the winner's rename had already published the loser's
// content). A crash can leave one unique temp behind — same trade-off node
// makes; it is never mistaken for a live draft.
pub(crate) async fn write_file_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = temp_path_for(path);
    let result = match tokio::fs::write(&tmp, data).await {
        Ok(()) => atomic_replace(&tmp, path).await,
        Err(e) => Err(e),
    };
    if result.is_err() {
        // Our own draft, our own cleanup: never leave a half-written temp.
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// Unique sibling temp file path for this write: `<path>.<pid>.<n>.tmp`.
fn temp_path_for(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".{}.{}.tmp", std::process::id(), n));
    PathBuf::from(tmp)
}

/// Read-modify-write `path` in ONE helper: read the current content, let `f`
/// produce the replacement (`None` = no change, nothing is written), then
/// publish through [`write_file_atomic`] so readers never observe a torn
/// file. Returns `f`'s tag on success, `None` when `f` declined to edit or
/// any step failed (unreadable file, failed write) — callers that treat
/// "0 replacements" and "write failed" alike just unwrap.
///
/// This is the generic atomic-edit seam: `replaceText` is the first caller;
/// any future native transform (trim, normalize, config re-save) belongs
/// here rather than open-coding read + write again.
pub(crate) async fn edit_file_atomic<T>(
    path: &Path,
    f: impl FnOnce(&[u8]) -> Option<(Vec<u8>, T)>,
) -> std::io::Result<Option<T>> {
    let data = tokio::fs::read(path).await?;
    let Some((out, tag)) = f(&data) else {
        return Ok(None);
    };
    write_file_atomic(path, &out).await?;
    Ok(Some(tag))
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
    .map_err(|e| std::io::Error::other(e.to_string()))?
    .map_err(|e| std::io::Error::other(format!("rename: {e}")))
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
        |cx, s| {
            cx.create_string_from_utf8(s.as_bytes().to_vec())
                .map(JSValue::make_string)
        },
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
        |cx, s| {
            cx.create_string_from_utf8(s.as_bytes().to_vec())
                .map(JSValue::make_string)
        },
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

            // ponytail: whole-file in memory (O(file)); stream if huge files matter.
            // edit_file_atomic owns read → transform → atomic publish.
            let result = edit_file_atomic(&path, |data| {
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
                                    return None;
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
                    return None;
                }
                out.extend_from_slice(&data[last..]);
                Some((out, count))
            })
            .await;

            // Unreadable file, declined edit, or failed write → 0, as before.
            Ok(result.unwrap_or(None).unwrap_or(0))
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

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    /// The whole point of the unique temp name: two writes to the same path
    /// must never draft into the same file (the old fixed `x.txt.tmp` made the
    /// loser's rename fail with ENOENT after the winner had already moved the
    /// shared temp — reporting failure for a write that had landed).
    #[test]
    fn temp_path_is_unique_per_call() {
        let target = Path::new("/some/dir/file.txt");
        let a = temp_path_for(target);
        let b = temp_path_for(target);
        assert_ne!(a, b, "two writes must not share a temp: {a:?} vs {b:?}");

        // Sibling of the target (same directory → same filesystem → the
        // rename stays atomic), carrying the pid so a second process cannot
        // collide either.
        assert_eq!(a.parent(), target.parent());
        let name = a.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("file.txt."), "got {name}");
        assert!(name.ends_with(".tmp"), "got {name}");
        assert!(
            name.contains(&format!(".{}.", std::process::id())),
            "pid embedded: {name}"
        );
    }

    /// A burst of writes yields all-distinct temps (pid fixed in-process, so
    /// the counter alone must separate them).
    #[test]
    fn temp_paths_of_a_burst_are_all_distinct() {
        let target = Path::new("x.txt");
        let seen: std::collections::HashSet<_> = (0..64).map(|_| temp_path_for(target)).collect();
        assert_eq!(seen.len(), 64, "counter must not repeat within a process");
    }

    /// Concurrent atomic writes to one path: every writer succeeds (no
    /// ENOENT on a stolen temp) and the final content is exactly one
    /// writer's complete payload — never a torn mix.
    #[tokio::test]
    async fn concurrent_writes_all_succeed_with_intact_content() {
        let dir = std::env::temp_dir().join(format!(
            "aike-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("t.txt");

        let writers: Vec<_> = (0..8)
            .map(|i| {
                let target = target.clone();
                let payload = format!("writer-{i}-{}", "x".repeat(1024 * 1024));
                tokio::spawn(async move { write_file_atomic(&target, payload.as_bytes()).await })
            })
            .collect();
        for w in writers {
            w.await
                .unwrap()
                .expect("concurrent atomic write must not fail with ENOENT");
        }

        let content = std::fs::read_to_string(&target).unwrap();
        let line = content.trim();
        assert!(
            line.starts_with("writer-")
                && line.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "one writer's complete content, no torn mix: {:?}",
            &line[..line.len().min(40)]
        );

        // No temp left behind after success (each rename consumed its own).
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "successful writes leave no temp files");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
