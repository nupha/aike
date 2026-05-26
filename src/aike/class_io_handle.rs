// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::Arc,
};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use crossbeam::channel::{Receiver, Sender};
use linkme::distributed_slice;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zuqe::{
    CallContext, ClassID, ErrorKind, FieldDef, JSResult, JSValue, JobState, Object, PollSender,
    Runtime, ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise, typed_array::TypedArray},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

const PROP_FD: i32 = 5;
const BUF_SIZE_STDIN: usize = 16384;
const BUF_SIZE_FILE: usize = 65536;
const BUF_SIZE_PIPE: usize = 4096;

/// Various IO handle: File, Pipe
pub(crate) enum IoPayload {
    Stdin,
    Stdout,
    Stderr,
    File(Arc<tokio::sync::Mutex<tokio::fs::File>>),
    PipeRead(
        Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>,
        i32,
    ),
    PipeWrite(
        Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncWrite + Unpin + Send>>>,
        i32,
    ),
}

impl IoPayload {
    fn read_buf_size(&self) -> usize {
        match self {
            Self::File(_) => BUF_SIZE_FILE,
            Self::Stdin => BUF_SIZE_STDIN,
            Self::Stdout | Self::Stderr => 0,
            Self::PipeRead(..) => BUF_SIZE_PIPE,
            Self::PipeWrite(..) => 0,
        }
    }

    fn as_raw_fd(&self) -> i32 {
        #[cfg(unix)]
        match self {
            Self::Stdin => std::io::stdin().as_raw_fd(),
            Self::Stdout => std::io::stdout().as_raw_fd(),
            Self::Stderr => std::io::stderr().as_raw_fd(),
            Self::File(fd) => fd.try_lock().map(|f| f.as_raw_fd()).unwrap_or(-1),
            Self::PipeRead(_, f) | Self::PipeWrite(_, f) => *f,
        }

        #[cfg(windows)]
        match self {
            Self::Stdin => 0,
            Self::Stdout => 1,
            Self::Stderr => 2,
            Self::OpenFile { .. } => -1,
            Self::PipeRead(..) | Self::PipeWrite(..) => -1,
        }
    }
}

// ---------------------------------------------------------------------------
// onRead — async read loop with callback, modeled after timeout.rs
// ---------------------------------------------------------------------------

enum OnReadMsg {
    Data(i32, Vec<u8>),
    Eof(i32),
    Error(i32, String),
}

struct OnReadEntry {
    callback: Object,
}

struct OnReadState {
    entries: HashMap<i32, OnReadEntry>,
    rx: Receiver<OnReadMsg>,
    tx: Sender<OnReadMsg>,
}

thread_local! {
    static ON_DATA_STATE: RefCell<Option<OnReadState>> = const { RefCell::new(None) };
}

pub(crate) fn gc_trace_io(tr: &mut GcTraceCtx) {
    ON_DATA_STATE.with_borrow(|opt| {
        if let Some(s) = opt.as_ref() {
            for entry in s.entries.values() {
                entry.callback.trace(tr);
            }
        }
    });
}

fn start_on_read_microtask(ctx: &mut ScopedContext) {
    ctx.queue_job(
        |cx| {
            let msg =
                ON_DATA_STATE.with_borrow(|opt| opt.as_ref().and_then(|s| s.rx.try_recv().ok()));

            match msg {
                None => {
                    let still_any = ON_DATA_STATE
                        .with_borrow(|opt| opt.as_ref().map_or(false, |s| !s.entries.is_empty()));
                    if still_any {
                        Ok(JobState::Again)
                    } else {
                        ON_DATA_STATE.with_borrow_mut(|opt| *opt = None);
                        Ok(JobState::Done)
                    }
                }
                Some(OnReadMsg::Data(fd, data)) => {
                    let cb = ON_DATA_STATE.with_borrow(|opt| {
                        opt.as_ref()
                            .and_then(|s| s.entries.get(&fd).map(|e| e.callback))
                    });
                    if let Some(cb) = cb {
                        if let Ok(arg) = TypedArray::create_from_vec_u8(cx, data).map(JSValue::from)
                        {
                            let _ = cb.call(cx, JSValue::UNDEFINED, &[arg]);
                        }
                    }
                    Ok(JobState::Again)
                }
                Some(OnReadMsg::Eof(fd)) => {
                    let cb = ON_DATA_STATE.with_borrow_mut(|opt| {
                        opt.as_mut()
                            .and_then(|s| s.entries.remove(&fd).map(|e| e.callback))
                    });
                    if let Some(cb) = cb {
                        let _ = cb.call(cx, JSValue::UNDEFINED, &[JSValue::NULL]);
                    }
                    let still_any = ON_DATA_STATE
                        .with_borrow(|opt| opt.as_ref().map_or(false, |s| !s.entries.is_empty()));
                    if still_any {
                        Ok(JobState::Again)
                    } else {
                        ON_DATA_STATE.with_borrow_mut(|opt| *opt = None);
                        Ok(JobState::Done)
                    }
                }
                Some(OnReadMsg::Error(fd, err_msg)) => {
                    let cb = ON_DATA_STATE.with_borrow_mut(|opt| {
                        opt.as_mut()
                            .and_then(|s| s.entries.remove(&fd).map(|e| e.callback))
                    });
                    if let Some(cb) = cb {
                        if let Some(err_obj) =
                            cx.create_error_object(ErrorKind::Internal, &err_msg, false)
                        {
                            let _ = cb.call(cx, JSValue::UNDEFINED, &[err_obj.into()]);
                        }
                    }
                    let still_any = ON_DATA_STATE
                        .with_borrow(|opt| opt.as_ref().map_or(false, |s| !s.entries.is_empty()));
                    if still_any {
                        Ok(JobState::Again)
                    } else {
                        ON_DATA_STATE.with_borrow_mut(|opt| *opt = None);
                        Ok(JobState::Done)
                    }
                }
            }
        },
        (),
    );
}

fn get_file_desc<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a IoPayload, i32> {
    val.as_native_object::<_, JSIoHandle>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_internal_error("invalid IoHandle instance");
            -1
        })
}

fn js_fh_onData(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;
    let raw_fd = desc.as_raw_fd();

    // Cannot read from stdout/stderr
    if matches!(desc, IoPayload::Stdout | IoPayload::Stderr) {
        ctx.throw_type_error("cannot read from stdout/stderr");
        return Err(-1);
    }

    let arg = cc.arg(0);
    if arg.is_null() {
        ON_DATA_STATE.with_borrow_mut(|opt| {
            if let Some(s) = opt.as_mut() {
                s.entries.remove(&raw_fd);
            }
        });
        return Ok(JSValue::UNDEFINED);
    }
    let callback = arg.into_callable(ctx)?;

    // Initialize state if needed
    let need_microtask = ON_DATA_STATE.with_borrow(|opt| opt.is_none());
    if need_microtask {
        let (tx, rx) = crossbeam::channel::unbounded();
        ON_DATA_STATE.with_borrow_mut(|opt| {
            *opt = Some(OnReadState {
                entries: HashMap::new(),
                rx,
                tx,
            });
        });
    }

    // Replace callback if already registered, don't re-spawn task
    let already_registered = ON_DATA_STATE.with_borrow(|opt| {
        opt.as_ref()
            .map_or(false, |s| s.entries.contains_key(&raw_fd))
    });

    ON_DATA_STATE.with_borrow_mut(|opt| {
        if let Some(s) = opt.as_mut() {
            s.entries.insert(raw_fd, OnReadEntry { callback });
        }
    });

    // Spawn async read loop for new registrations
    if !already_registered {
        let tx = ON_DATA_STATE.with_borrow(|opt| opt.as_ref().unwrap().tx.clone());

        match desc {
            IoPayload::Stdin => {
                let buf_size = desc.read_buf_size();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; buf_size];
                    let mut stdin = tokio::io::stdin();
                    loop {
                        match stdin.read(&mut buf).await {
                            Ok(0) => {
                                let _ = tx.send(OnReadMsg::Eof(raw_fd));
                                break;
                            }
                            Ok(n) => {
                                let _ = tx.send(OnReadMsg::Data(raw_fd, buf[..n].to_vec()));
                            }
                            Err(e) => {
                                let _ = tx.send(OnReadMsg::Error(raw_fd, e.to_string()));
                                break;
                            }
                        }
                    }
                });
            }
            IoPayload::File(fd) => {
                let fd = fd.clone();
                let buf_size = desc.read_buf_size();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; buf_size];
                    loop {
                        let result = {
                            let mut file = fd.lock().await;
                            file.read(&mut buf).await
                        };
                        match result {
                            Ok(0) => {
                                let _ = tx.send(OnReadMsg::Eof(raw_fd));
                                break;
                            }
                            Ok(n) => {
                                let _ = tx.send(OnReadMsg::Data(raw_fd, buf[..n].to_vec()));
                            }
                            Err(e) => {
                                let _ = tx.send(OnReadMsg::Error(raw_fd, e.to_string()));
                                break;
                            }
                        }
                    }
                });
            }
            IoPayload::PipeRead(reader, _) => {
                let reader = reader.clone();
                let buf_size = BUF_SIZE_PIPE;
                tokio::spawn(async move {
                    let mut buf = vec![0u8; buf_size];
                    loop {
                        let result = {
                            let mut r = reader.lock().await;
                            r.read(&mut buf).await
                        };
                        match result {
                            Ok(0) => {
                                let _ = tx.send(OnReadMsg::Eof(raw_fd));
                                break;
                            }
                            Ok(n) => {
                                let _ = tx.send(OnReadMsg::Data(raw_fd, buf[..n].to_vec()));
                            }
                            Err(e) => {
                                let _ = tx.send(OnReadMsg::Error(raw_fd, e.to_string()));
                                break;
                            }
                        }
                    }
                });
            }
            IoPayload::PipeWrite(..) => {
                let _ = tx.send(OnReadMsg::Error(
                    raw_fd,
                    "cannot read from pipe writer".to_string(),
                ));
            }
            _ => unreachable!(),
        }
    }

    // Start the polling microtask if state was just created
    if need_microtask {
        start_on_read_microtask(ctx);
    }

    Ok(JSValue::UNDEFINED)
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
        IoPayload::Stdin => {
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
        IoPayload::Stdout | IoPayload::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        IoPayload::File(fd) => {
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
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut r = reader.lock().await;
                let mut buf = String::new();
                let result = r
                    .read_to_string(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeWrite(..) => {
            let _ = tx.send(Err("cannot read from pipe writer".to_string()));
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
        IoPayload::Stdin => {
            tokio::spawn(async move {
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
        IoPayload::Stdout | IoPayload::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        IoPayload::File(fd) => {
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
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut r = reader.lock().await;
                let mut buf = String::new();
                let raw = r
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
        IoPayload::PipeWrite(..) => {
            let _ = tx.send(Err("cannot read from pipe writer".to_string()));
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
        IoPayload::Stdin => {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let result = tokio::io::stdin()
                    .read_to_end(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::Stdout | IoPayload::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        IoPayload::File(fd) => {
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
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut r = reader.lock().await;
                let mut buf = Vec::new();
                let result = r
                    .read_to_end(&mut buf)
                    .await
                    .map(|_| buf)
                    .map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeWrite(..) => {
            let _ = tx.send(Err("cannot read from pipe writer".to_string()));
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
        IoPayload::Stdin | IoPayload::Stdout | IoPayload::Stderr => {
            // std streams: stat via /dev/stdin etc. is unreliable, return zero-stats
            let _ = tx.send(Err("cannot stat standard stream".to_string()));
        }
        IoPayload::File(fd) => {
            let fd = Arc::clone(fd);
            tokio::spawn(async move {
                let file = fd.lock().await;
                let result = file.metadata().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeRead(..) | IoPayload::PipeWrite(..) => {
            let _ = tx.send(Err("cannot stat pipe".to_string()));
        }
    }

    Ok(promise.into())
}

/// read(maxCount?) — async read from fd at current position, returns Uint8Array
fn js_fh_read(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
        IoPayload::Stdin => {
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
        IoPayload::Stdout | IoPayload::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        IoPayload::File(fd) => {
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
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut r = reader.lock().await;
                let result = if max_len == 0 {
                    r.read_to_end(&mut buf)
                        .await
                        .map(|_| buf)
                        .map_err(|e| e.to_string())
                } else {
                    buf.resize(max_len, 0);
                    r.read(&mut buf)
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
        IoPayload::PipeWrite(..) => {
            let _ = tx.send(Err("cannot read from pipe writer".to_string()));
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
        IoPayload::Stdin => {
            let _ = tx.send(Err("cannot write to stdin".to_string()));
        }
        IoPayload::Stdout | IoPayload::Stderr => {
            let is_stderr = matches!(desc, IoPayload::Stderr);
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
        IoPayload::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;
                let result = file.write_all(&data).await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeWrite(writer, _) => {
            let writer = writer.clone();
            tokio::spawn(async move {
                let mut w = writer.lock().await;
                let result = w.write_all(&data).await.map_err(|e| e.to_string());
                let _ = w.flush().await;
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeRead(..) => {
            let _ = tx.send(Err("cannot write to pipe reader".to_string()));
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
        IoPayload::Stdin => {
            let _ = tx.send(Ok(()));
        }
        IoPayload::Stdout | IoPayload::Stderr => {
            let is_stderr = matches!(desc, IoPayload::Stderr);
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
        IoPayload::File(fd) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut file = fd.lock().await;
                let result = file.flush().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeWrite(writer, _) => {
            let writer = writer.clone();
            tokio::spawn(async move {
                let mut w = writer.lock().await;
                let result = w.flush().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
        }
        IoPayload::PipeRead(..) => {
            let _ = tx.send(Ok(()));
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
                drop(Box::from_raw(p.cast::<IoPayload>()));
            }
        }

        Ok(JSValue::UNDEFINED)
    } else {
        ctx.throw_type_error_not_an_object();
        Err(-1)
    }
}

// ---------------------------------------------------------------------------
// stat helper — shared between IoHandle.stat and fs.stat
// ---------------------------------------------------------------------------

pub(crate) fn make_stats(
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

// ---------------------------------------------------------------------------
// [Symbol.asyncIterator] — `for await...of` support
// ---------------------------------------------------------------------------

/// Chunk message type for IoHandle async iteration
enum FileChunk {
    Data(Vec<u8>),
    Eof,
    Error(String),
}

/// [Symbol.asyncIterator]() — returns an AsyncIterableIterator for `for await...of`
fn js_fh_asyncIterator(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_file_desc(ctx, &this)?;

    if matches!(desc, IoPayload::Stdout | IoPayload::Stderr) {
        ctx.throw_type_error("cannot iterate stdout/stderr");
        return Err(-1);
    }

    let (tx, rx) = crossbeam::channel::bounded::<FileChunk>(8);

    match desc {
        IoPayload::Stdin => {
            let buf_size = desc.read_buf_size();
            tokio::spawn(async move {
                let mut buf = vec![0u8; buf_size];
                let mut stdin = tokio::io::stdin();
                loop {
                    match stdin.read(&mut buf).await {
                        Ok(0) => {
                            let _ = tx.send(FileChunk::Eof);
                            break;
                        }
                        Ok(n) => {
                            let _ = tx.send(FileChunk::Data(buf[..n].to_vec()));
                        }
                        Err(e) => {
                            let _ = tx.send(FileChunk::Error(e.to_string()));
                            break;
                        }
                    }
                }
            });
        }
        IoPayload::File(fd) => {
            let fd = fd.clone();
            let buf_size = desc.read_buf_size();
            tokio::spawn(async move {
                let mut buf = vec![0u8; buf_size];
                loop {
                    let result = {
                        let mut file = fd.lock().await;
                        file.read(&mut buf).await
                    };
                    match result {
                        Ok(0) => {
                            let _ = tx.send(FileChunk::Eof);
                            break;
                        }
                        Ok(n) => {
                            let _ = tx.send(FileChunk::Data(buf[..n].to_vec()));
                        }
                        Err(e) => {
                            let _ = tx.send(FileChunk::Error(e.to_string()));
                            break;
                        }
                    }
                }
            });
        }
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            let buf_size = BUF_SIZE_PIPE;
            tokio::spawn(async move {
                let mut buf = vec![0u8; buf_size];
                loop {
                    let result = {
                        let mut r = reader.lock().await;
                        r.read(&mut buf).await
                    };
                    match result {
                        Ok(0) => {
                            let _ = tx.send(FileChunk::Eof);
                            break;
                        }
                        Ok(n) => {
                            let _ = tx.send(FileChunk::Data(buf[..n].to_vec()));
                        }
                        Err(e) => {
                            let _ = tx.send(FileChunk::Error(e.to_string()));
                            break;
                        }
                    }
                }
            });
        }
        IoPayload::PipeWrite(..) => {
            let _ = tx.send(FileChunk::Error("cannot iterate pipe writer".to_string()));
        }
        _ => unreachable!(),
    }

    let iter = AsyncIterableIterator::create_with_receiver::<FileChunk>(
        ctx,
        rx,
        |cx, chunk| -> Result<Option<JSValue>, i32> {
            match chunk {
                FileChunk::Data(data) => TypedArray::create_from_vec_u8(cx, data)
                    .map(JSValue::from)
                    .map(Some),
                FileChunk::Eof => Ok(None),
                FileChunk::Error(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        },
    )?;

    Ok(iter.into())
}

const CLASS_IO_HANDLE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("fd", Some(js_fh_fd), None, PROP_FD as _),
    FieldDef::FUNC("text", 0, js_fh_text, 0),
    FieldDef::FUNC("json", 0, js_fh_json, 0),
    FieldDef::FUNC("bytes", 0, js_fh_bytes, 0),
    FieldDef::FUNC("read", 1, js_fh_read, 0),
    FieldDef::FUNC("stat", 0, js_fh_stat, 0),
    FieldDef::FUNC("write", 1, js_fh_writeAll, 0),
    FieldDef::FUNC("flush", 0, js_fh_flush, 0),
    FieldDef::FUNC("close", 0, js_fh_close, 0),
    FieldDef::FUNC("onData", 1, js_fh_onData, 0),
    FieldDef::FUNC("[Symbol.asyncIterator]", 0, js_fh_asyncIterator, 0),
];

def_native_object!("IoHandle", JSIoHandle, IoPayload, CLASS_IO_HANDLE_PROTO);

impl JSIoHandle {
    pub fn new_instance_stdin(ctx: &mut ScopedContext) -> Result<Object, i32> {
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::Stdin)
    }

    pub fn new_instance_stdout(ctx: &mut ScopedContext) -> Result<Object, i32> {
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::Stdout)
    }

    pub fn new_instance_stderr(ctx: &mut ScopedContext) -> Result<Object, i32> {
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::Stderr)
    }

    /// Create a IoHandle from an already-opened tokio::fs::File
    pub fn new_instance_file(
        ctx: &mut ScopedContext,
        new_target: JSValue,
        file: tokio::fs::File,
    ) -> Result<Object, i32> {
        Self::new_instance(
            ctx,
            new_target,
            IoPayload::File(Arc::new(tokio::sync::Mutex::new(file))),
        )
    }

    /// Create a IoHandle from an async pipe reader (ChildStdout/ChildStderr).
    /// `fd` is the platform-specific file descriptor (used for the `.fd` property).
    pub fn new_instance_pipe_read(
        ctx: &mut ScopedContext,
        reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        fd: i32,
    ) -> Result<Object, i32> {
        Self::new_instance(
            ctx,
            JSValue::UNDEFINED,
            IoPayload::PipeRead(Arc::new(tokio::sync::Mutex::new(Box::new(reader))), fd),
        )
    }

    /// Create a IoHandle from an async pipe writer (ChildStdin).
    /// `fd` is the platform-specific file descriptor (used for the `.fd` property).
    pub fn new_instance_pipe_write(
        ctx: &mut ScopedContext,
        writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
        fd: i32,
    ) -> Result<Object, i32> {
        Self::new_instance(
            ctx,
            JSValue::UNDEFINED,
            IoPayload::PipeWrite(Arc::new(tokio::sync::Mutex::new(Box::new(writer))), fd),
        )
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_io_handle(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSIoHandle::register(rt)
}
