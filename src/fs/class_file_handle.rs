// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, sync::Arc};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, PollSender, Runtime,
    ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::USER_CLASS_DEFS;

const PROP_FD: i32 = 5;

enum FileHandleDesc {
    Stdin,
    Stdout,
    Stderr,
    File(Arc<tokio::sync::Mutex<tokio::fs::File>>),
}

impl FileHandleDesc {
    fn as_raw_fd(&self) -> i32 {
        #[cfg(unix)]
        match self {
            Self::Stdin => std::io::stdin().as_raw_fd(),
            Self::Stdout => std::io::stdout().as_raw_fd(),
            Self::Stderr => std::io::stderr().as_raw_fd(),
            Self::File(fd) => fd.try_lock().map(|f| f.as_raw_fd()).unwrap_or(-1),
        }

        #[cfg(windows)]
        match self {
            Self::Stdin => 0,
            Self::Stdout => 1,
            Self::Stderr => 2,
            Self::OpenFile { .. } => -1,
        }
    }
}

fn get_file_desc<'a>(ctx: &mut ScopedContext, val: &JSValue) -> Result<&'a FileHandleDesc, i32> {
    if let Some(obj) = val.as_object() {
        let opaque = obj
            .retrive_payload(ctx, obj.class_id())?
            .other()
            .ok_or_else(|| -1)?
            .opaque();

        if !opaque.is_null() {
            Ok(unsafe { &*opaque.cast::<FileHandleDesc>() })
        } else {
            ctx.throw_internal_error("invalid FileHandle");
            Err(-1)
        }
    } else {
        ctx.throw_type_error("expected FileHandle object");
        Err(-1)
    }
}

/// text() — async read as string via the real fd
fn js_fh_text(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(ctx, |cx, payload| -> JSResult {
            let text = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::from)
        })?;

    match desc {
        FileHandleDesc::Stdin => {
            tokio::spawn(async move {
                let mut buf = String::new();
                let result = tokio::io::stdin()
                    .read_to_string(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        FileHandleDesc::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;
                let _ = file.seek(std::io::SeekFrom::Start(0)).await;
                let mut buf = String::new();
                let result = file
                    .read_to_string(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// json() — async read and parse as JSON via the real fd
fn js_fh_json(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

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

    match desc {
        FileHandleDesc::Stdin => {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let raw = tokio::io::stdin()
                    .read_to_string(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let result = match raw {
                    Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                };
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        FileHandleDesc::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut file = fd.lock().await;
                let _ = file.seek(std::io::SeekFrom::Start(0)).await;
                let mut buf = String::new();
                let raw = file
                    .read_to_string(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let result = match raw {
                    Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
                    Err(e) => Err(e),
                };
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// bytes() — async read as Uint8Array via the real fd
fn js_fh_bytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from)
        })?;

    match desc {
        FileHandleDesc::Stdin => {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = Vec::new();
                let result = tokio::io::stdin()
                    .read_to_end(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        FileHandleDesc::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                let mut file = fd.lock().await;
                let _ = file.seek(std::io::SeekFrom::Start(0)).await;
                let mut buf = Vec::new();
                let result = file
                    .read_to_end(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// stat() — async get file metadata via the fd
fn js_fh_stat(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

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

    match desc {
        FileHandleDesc::Stdin | FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            // std streams: stat via /dev/stdin etc. is unreliable, return zero-stats
            let _ = tx.send(Err("cannot stat standard stream".to_string()));
        }
        FileHandleDesc::File(fd) => {
            let fd = Arc::clone(fd);
            tokio::spawn(async move {
                let file = fd.lock().await;
                let result = file.metadata().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// read(maxCount?) — async read from fd at current position, returns Uint8Array
fn js_fh_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    use tokio::io::AsyncReadExt;

    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

    // Optional length arg; 0 = read to EOF
    let max_len: usize = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(ctx)?.to_i32().max(0) as usize
    } else {
        0
    };

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from)
        })?;

    let mut buf = Vec::with_capacity(max_len);

    match desc {
        FileHandleDesc::Stdin => {
            tokio::spawn(async move {
                let result = if max_len == 0 {
                    tokio::io::stdin()
                        .read_to_end(&mut buf)
                        .await
                        .map(|_| buf)
                        .map_err(|e| e.to_string())
                } else {
                    buf.resize(max_len, 0);
                    tokio::io::stdin()
                        .read(&mut buf)
                        .await
                        .map(|n| {
                            buf.truncate(n);
                            buf
                        })
                        .map_err(|e| e.to_string())
                };
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        FileHandleDesc::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;

                let result = if max_len == 0 {
                    file.read_to_end(&mut buf)
                        .await
                        .map(|_| buf)
                        .map_err(|e| e.to_string())
                } else {
                    buf.resize(max_len, 0);
                    file.read(&mut buf)
                        .await
                        .map(|n| {
                            buf.truncate(n);
                            buf
                        })
                        .map_err(|e| e.to_string())
                };
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// async write all
fn js_fh_writeAll(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    use tokio::io::AsyncWriteExt;

    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

    let arg = cc.arg(0);

    // Extract bytes: TypedArray, ArrayBuffer or String
    let data: Vec<u8> = if let Some(s) = arg.as_string() {
        s.content().encode_utf8(false).into_owned().into_bytes()
    } else if let Some(p) = arg.as_object().map(|o| o.payload()) {
        if let Some(ta) = p.typed_array() {
            ta.bytes().to_vec()
        } else if let Some(ab) = p.array_buffer() {
            ab.bytes().to_vec()
        } else {
            ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument");
            return Err(-1);
        }
    } else {
        ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument");
        return Err(-1);
    };

    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
            payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::UNDEFINED)
        })?;

    // TODO: put arg to a gc tracing list, and move bytes raw ptr and length into async task.

    match desc {
        FileHandleDesc::Stdin => {
            let _ = tx.send(Err("cannot write to stdin".to_string()));
        }
        FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            let is_stderr = matches!(desc, FileHandleDesc::Stderr);
            tokio::spawn(async move {
                let mut writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = if is_stderr {
                    Box::new(tokio::io::stderr())
                } else {
                    Box::new(tokio::io::stdout())
                };
                let result = writer.write_all(&data).await.map_err(|e| e.to_string());
                let _ = writer.flush().await;
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        FileHandleDesc::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;
                let result = file.write_all(&data).await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// flush() — async flush output
fn js_fh_flush(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
            payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::UNDEFINED)
        })?;

    match desc {
        FileHandleDesc::Stdin => {
            let _ = tx.send(Ok(()));
        }
        FileHandleDesc::Stdout | FileHandleDesc::Stderr => {
            let is_stderr = matches!(desc, FileHandleDesc::Stderr);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut writer: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = if is_stderr {
                    Box::new(tokio::io::stderr())
                } else {
                    Box::new(tokio::io::stdout())
                };
                let result = writer.flush().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        FileHandleDesc::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut file = fd.lock().await;
                let result = file.flush().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
    }

    Ok(promise.into())
}

/// close() — sync close: drop the fd
fn js_fh_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();

    if let Some(o) = this.as_object() {
        let u = this
            .retrive_object_payload_mut(ctx, o.class_id())?
            .other_mut()
            .unwrap();

        if !u.opaque().is_null() {
            unsafe {
                let p = u.replace_opaque(std::ptr::null_mut());
                drop(Box::from_raw(p.cast::<FileHandleDesc>()));
            }
        }

        Ok(JSValue::UNDEFINED)
    } else {
        ctx.throw_type_error_not_an_object();
        Err(-1)
    }
}

// ---------------------------------------------------------------------------
// stat helper — shared between FileHandle.stat and fs.stat
// ---------------------------------------------------------------------------

pub(super) fn make_stats(
    ctx: &mut ScopedContext,
    meta: &std::fs::Metadata,
) -> Result<JSValue, i32> {
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

fn js_fh_fd(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;
    Ok(JSValue::Int(desc.as_raw_fd()))
}

const CLASS_FILE_HANDLE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("fd", Some(js_fh_fd), None, PROP_FD as _),
    FieldDef::CALL("text", 0, js_fh_text, 0),
    FieldDef::CALL("json", 0, js_fh_json, 0),
    FieldDef::CALL("bytes", 0, js_fh_bytes, 0),
    FieldDef::CALL("read", 1, js_fh_read, 0),
    FieldDef::CALL("stat", 0, js_fh_stat, 0),
    FieldDef::CALL("write", 1, js_fh_writeAll, 0),
    FieldDef::CALL("flush", 0, js_fh_flush, 0),
    FieldDef::CALL("close", 0, js_fh_close, 0),
];

pub struct JSFileHandle;

impl JSFileHandle {
    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt
            .register_class(
                ClassDef::new("FileHandle", CLASS_FILE_HANDLE_PROTO, None).on_dispose(
                    |_, opaque, _| {
                        if !opaque.is_null() {
                            let _ = unsafe { Box::from_raw(opaque.cast::<FileHandleDesc>()) };
                        }
                    },
                ),
            )
            .expect("register class FileHandle failed");

        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    fn new_instance_internal(
        ctx: &mut ScopedContext,
        new_target: JSValue,
        desc: FileHandleDesc,
    ) -> Result<Object, i32> {
        let class_id = Self::class_id();
        let opaque = Box::into_raw(Box::new(desc)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(new_target, class_id, opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<FileHandleDesc>()) };
                -1
            })
    }

    pub fn new_instance_stdin(ctx: &mut ScopedContext) -> Result<Object, i32> {
        Self::new_instance_internal(ctx, JSValue::UNDEFINED, FileHandleDesc::Stdin)
    }

    pub fn new_instance_stdout(ctx: &mut ScopedContext) -> Result<Object, i32> {
        Self::new_instance_internal(ctx, JSValue::UNDEFINED, FileHandleDesc::Stdout)
    }

    pub fn new_instance_stderr(ctx: &mut ScopedContext) -> Result<Object, i32> {
        Self::new_instance_internal(ctx, JSValue::UNDEFINED, FileHandleDesc::Stderr)
    }

    /// Create a FileHandle from an already-opened tokio::fs::File
    pub fn new_instance(
        ctx: &mut ScopedContext,
        new_target: JSValue,
        file: tokio::fs::File,
    ) -> Result<Object, i32> {
        Self::new_instance_internal(
            ctx,
            new_target,
            FileHandleDesc::File(Arc::new(tokio::sync::Mutex::new(file))),
        )
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_file_handle(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSFileHandle::register(rt)
}
