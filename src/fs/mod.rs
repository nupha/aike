// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

pub(crate) mod class_file_handle;

use zuqe::{
    Array, CallContext, FieldDef, JSResult, JSValue, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::fs::class_file_handle::{JSFileHandle, handle_get_path};

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

/// Send a value through a crossbeam channel from an async context.
/// Uses try_send in a loop to avoid blocking the async task.
/// If the channel is full, yields to the tokio runtime and retries.
pub(super) async fn async_send<T>(
    tx: &crossbeam::channel::Sender<T>,
    val: T,
) -> Result<(), crossbeam::channel::TrySendError<T>> {
    let mut val = val;
    loop {
        match tx.try_send(val) {
            Ok(()) => {
                return Ok(());
            }
            Err(crossbeam::channel::TrySendError::Full(v)) => {
                val = v;
                tokio::task::yield_now().await;
            }
            Err(crossbeam::channel::TrySendError::Disconnected(v)) => {
                return Err(crossbeam::channel::TrySendError::Disconnected(v));
            }
        }
    }
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

// ---------------------------------------------------------------------------
// readText - read file content as UTF-8 string
// ---------------------------------------------------------------------------
fn js_fs_readText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();

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
        async_send(
            &tx,
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
    let path = v.content().encode_utf8(false).to_string();

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
        async_send(&tx, result).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// readBytes - read file as binary data (Uint8Array)
// ---------------------------------------------------------------------------
fn js_fs_readBytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        async_send(&tx, tokio::fs::read(&path).await.map_err(|e| e.to_string())).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// writeText - write string to file
// ---------------------------------------------------------------------------
fn js_fs_writeText(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();
    let data = cc.arg(1).to_string(ctx)?;
    let data_str = data.content().encode_utf8(false).to_string();

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        async_send(
            &tx,
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
    let path = v.content().encode_utf8(false).to_string();
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
        async_send(
            &tx,
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
    let path = v.content().encode_utf8(false).to_string();
    let bytes = get_bytes_from_arg(ctx, &mut cc.arg(1))?;

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        async_send(
            &tx,
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
    let path = v.content().encode_utf8(false).to_string();
    let data = cc.arg(1).to_string(ctx)?;
    let data_str = data.content().encode_utf8(false).to_string();

    let (promise, tx) = make_void_promise(ctx)?;

    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        match tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await
        {
            Ok(mut f) => {
                let r: Result<(), String> = f
                    .write_all(data_str.as_bytes())
                    .await
                    .map_err(|e| e.to_string());
                async_send(&tx, r).await;
            }
            Err(e) => {
                async_send(&tx, Err(e.to_string())).await;
            }
        }
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// createFile - create a lazy file handle
// ---------------------------------------------------------------------------
fn js_fs_createFile(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();

    JSFileHandle::new_instance(ctx, path).map(JSValue::Object)
}

// ---------------------------------------------------------------------------
// write - multi-format unified write
// ---------------------------------------------------------------------------
fn js_fs_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();

    // string data
    if let Some(s) = cc.arg(1).as_string() {
        let content = s.content().encode_utf8(false).to_string();
        let (promise, tx) = make_void_promise(ctx)?;
        tokio::spawn(async move {
            async_send(
                &tx,
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
                async_send(
                    &tx,
                    tokio::fs::write(&path, &bytes)
                        .await
                        .map_err(|e| e.to_string()),
                )
                .await;
            });
            return Ok(promise.into());
        }
        Err(_) => {
            // fall through to FileHandle check
        }
    }

    // FileHandle: read from source, write to dest
    if JSFileHandle::is_instance(ctx, &cc.arg(1)) {
        let src_path = handle_get_path(ctx, &cc.arg(1));
        let (promise, tx) = make_void_promise(ctx)?;
        tokio::spawn(async move {
            let data = tokio::fs::read(&src_path).await.map_err(|e| e.to_string());
            match data {
                Ok(bytes) => {
                    async_send(
                        &tx,
                        tokio::fs::write(&path, &bytes)
                            .await
                            .map_err(|e| e.to_string()),
                    )
                    .await;
                }
                Err(e) => {
                    async_send(&tx, Err(e)).await;
                }
            }
        });
        return Ok(promise.into());
    }

    ctx.throw_type_error("unsupported data type for write");
    Err(-1)
}

// ---------------------------------------------------------------------------
// ensureDir - ensure directory exists
// ---------------------------------------------------------------------------
fn js_fs_ensureDir(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();

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
        async_send(&tx, r).await;
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
    let path = v.content().encode_utf8(false).to_string();

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
                let meta = entry.metadata().await.map_err(|e| e.to_string())?;
                entries.push(DirentData {
                    name,
                    size: meta.len(),
                    is_file: meta.is_file(),
                    is_dir: meta.is_dir(),
                    is_symlink: meta.is_symlink(),
                });
            }
            Ok(entries)
        }
        .await;
        async_send(&tx, r).await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// stat - get file stats
// ---------------------------------------------------------------------------
fn make_stats(ctx: &mut ScopedContext, meta: &std::fs::Metadata) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;

    let size = JSValue::from(meta.len() as i64);
    obj.define_property_value_str(ctx, "size", size, zuqe::JS_PROP_ENUMERABLE)?;

    if let Ok(created) = meta.created() {
        let ms = created
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64;
        obj.define_property_value_str(
            ctx,
            "createdAt",
            JSValue::from(ms),
            zuqe::JS_PROP_ENUMERABLE,
        )?;
    }

    if let Ok(modified) = meta.modified() {
        let ms = modified
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64;
        obj.define_property_value_str(
            ctx,
            "modifiedAt",
            JSValue::from(ms),
            zuqe::JS_PROP_ENUMERABLE,
        )?;
    }

    if let Ok(accessed) = meta.accessed() {
        let ms = accessed
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as f64;
        obj.define_property_value_str(
            ctx,
            "accessedAt",
            JSValue::from(ms),
            zuqe::JS_PROP_ENUMERABLE,
        )?;
    }

    obj.define_property_value_str(
        ctx,
        "isFile",
        JSValue::Bool(meta.is_file()),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isDirectory",
        JSValue::Bool(meta.is_dir()),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isSymbolicLink",
        JSValue::Bool(meta.is_symlink()),
        zuqe::JS_PROP_ENUMERABLE,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        obj.define_property_value_str(
            ctx,
            "mode",
            JSValue::from(meta.mode() as i64),
            zuqe::JS_PROP_ENUMERABLE,
        )?;
        obj.define_property_value_str(
            ctx,
            "uid",
            JSValue::from(meta.uid() as i64),
            zuqe::JS_PROP_ENUMERABLE,
        )?;
        obj.define_property_value_str(
            ctx,
            "gid",
            JSValue::from(meta.gid() as i64),
            zuqe::JS_PROP_ENUMERABLE,
        )?;
    }

    Ok(obj.into())
}

fn js_fs_stat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let v = cc.arg(0).to_string(ctx)?;
    let path = v.content().encode_utf8(false).to_string();

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
        async_send(
            &tx,
            tokio::fs::metadata(&path).await.map_err(|e| e.to_string()),
        )
        .await;
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Module definition
// ---------------------------------------------------------------------------
const MOD_FS_ENTRIES: &[FieldDef] = &[
    FieldDef::CALL("createFile", 1, js_fs_createFile, 0),
    FieldDef::CALL("readText", 1, js_fs_readText, 0),
    FieldDef::CALL("readJSON", 1, js_fs_readJSON, 0),
    FieldDef::CALL("readBytes", 1, js_fs_readBytes, 0),
    FieldDef::CALL("writeText", 2, js_fs_writeText, 0),
    FieldDef::CALL("writeJSON", 2, js_fs_writeJSON, 0),
    FieldDef::CALL("writeBytes", 2, js_fs_writeBytes, 0),
    FieldDef::CALL("appendText", 2, js_fs_appendText, 0),
    FieldDef::CALL("write", 2, js_fs_write, 0),
    FieldDef::CALL("ensureDir", 2, js_fs_ensureDir, 0),
    FieldDef::CALL("stat", 1, js_fs_stat, 0),
];

pub fn init_mod_fs(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("fs", MOD_FS_ENTRIES)?;
    Ok(())
}
