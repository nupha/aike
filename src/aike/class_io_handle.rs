// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    collections::HashMap,
    io::Write,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crossbeam::channel::{Receiver, Sender};
use linkme::distributed_slice;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zuqe::{
    CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, PollSender, Runtime,
    ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise, typed_array::TypedArray},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

/// RAII guard for a pipe fd — closes the fd on drop.
#[cfg(unix)]
pub(crate) struct PipeFd(i32);
#[cfg(unix)]
impl Drop for PipeFd {
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}
#[cfg(unix)]
impl PipeFd {
    fn as_virt_fd(&self) -> i32 {
        self.0
    }
}

/// No-op fallback for non-Unix platforms.
#[cfg(not(unix))]
struct PipeFd(i32);
#[cfg(not(unix))]
impl PipeFd {
    fn as_virt_fd(&self) -> i32 {
        self.0
    }
}

const PROP_FD: i32 = 5;
const BUF_SIZE_STDIN: usize = 16384;
const BUF_SIZE_FILE: usize = 65536;
const BUF_SIZE_PIPE: usize = 4096;

/// Allocate a monotonically increasing virtual fd for stable identification.
/// Used as a fallback when the OS fd is temporarily unavailable (e.g. File
/// locked by a concurrent reader).
fn alloc_fd() -> i32 {
    static NEXT_VIRTUAL_FD: AtomicI32 = AtomicI32::new(256);
    NEXT_VIRTUAL_FD.fetch_add(1, Ordering::Relaxed)
}

/// Various IO handle: File, Pipe
pub(crate) enum IoPayload {
    Stdin,
    Stdout,
    Stderr,
    File(Arc<tokio::sync::Mutex<tokio::fs::File>>, i32),
    PipeRead(
        Arc<tokio::sync::Mutex<Box<dyn tokio::io::AsyncRead + Unpin + Send>>>,
        i32,
    ),
    PipeWrite(PipeFd),
}

impl IoPayload {
    fn read_buf_size(&self) -> usize {
        match self {
            Self::File(..) => BUF_SIZE_FILE,
            Self::PipeRead(..) => BUF_SIZE_PIPE,
            Self::Stdin => BUF_SIZE_STDIN,
            Self::Stdout | Self::Stderr | Self::PipeWrite(..) => 0,
        }
    }

    fn as_virt_fd(&self) -> i32 {
        #[cfg(unix)]
        match self {
            Self::Stdin => std::io::stdin().as_raw_fd(),
            Self::Stdout => std::io::stdout().as_raw_fd(),
            Self::Stderr => std::io::stderr().as_raw_fd(),
            Self::File(fd, virt_fd) => fd.try_lock().map(|f| f.as_raw_fd()).unwrap_or(*virt_fd),
            Self::PipeRead(_, f) => *f,
            Self::PipeWrite(fd) => fd.as_virt_fd(),
        }

        #[cfg(windows)]
        match self {
            Self::Stdin => 0,
            Self::Stdout => 1,
            Self::Stderr => 2,
            Self::File(_, virt_fd) => *virt_fd,
            Self::PipeRead(_, f) => *f,
            Self::PipeWrite(fd) => fd.as_virt_fd(),
        }
    }
}

/// Max bytes of a single line accepted in Line mode — longer lines are dropped
/// (parity with ravenclaw's NDJSON framing).
const MAX_LINE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Unified read subscription — ONE consumer per readable handle.
//
// A single byte-pump task per fd feeds raw chunks into one channel; framing
// is applied at dispatch according to the active subscription mode:
//
//   onData → Data mode: raw chunks delivered as Uint8Array
//   onLine → Line mode: split at \n, one trailing \r stripped (CRLF), empty /
//            >MAX_LINE_BYTES lines dropped; at EOF the partial tail line is
//            flushed first, then callback(null) signals end-of-stream
//
// Registering the other mode while a subscription is active throws TypeError;
// `null` unsubscribes AND pauses the pump (nothing is consumed while nobody
// listens, so a later subscriber still sees the buffered data). The pump
// lives from first subscribe until EOF/error, and is reused across
// re-subscribes — there are never two concurrent readers on one handle.
// ---------------------------------------------------------------------------

enum PumpMsg {
    Chunk(i32, Vec<u8>),
    Eof(i32),
    Error(i32, String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadMode {
    Data,
    Line,
}

impl ReadMode {
    fn name(self) -> &'static str {
        match self {
            ReadMode::Data => "onData",
            ReadMode::Line => "onLine",
        }
    }
}

struct ReadSlot {
    mode: ReadMode,
    callback: Object,
    /// Line mode: raw bytes not yet terminated by `\n`.
    line_pending: Vec<u8>,
}

/// Pause/resume control shared with the pump task. `dead` is written only by
/// the dispatcher (on Eof/Error) — single writer, no races with re-subscribe.
#[derive(Clone)]
struct PumpControl {
    active: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    dead: Arc<AtomicBool>,
}

impl PumpControl {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
            wake: Arc::new(tokio::sync::Notify::new()),
            dead: Arc::new(AtomicBool::new(false)),
        }
    }
}

thread_local! {
    /// Active subscriptions (at most one per fd). Wiped when the poller stops.
    static READ_SLOTS: RefCell<HashMap<i32, ReadSlot>> = RefCell::new(HashMap::new());
    /// Pump controls — persist across poller generations so a handle never
    /// accumulates a second concurrent reader.
    static PUMPS: RefCell<HashMap<i32, PumpControl>> = RefCell::new(HashMap::new());
}

/// Process-global pump channel. The poller may stop and restart freely; pump
/// tasks keep sending into THIS receiver regardless of generation.
static PUMP_CHANNEL: LazyLock<(Sender<PumpMsg>, Receiver<PumpMsg>)> =
    LazyLock::new(|| crossbeam::channel::bounded(256));

/// Whether a read-poller microtask is currently queued/running.
static READ_POLLER_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn gc_trace_io(tr: &mut GcTraceCtx) {
    READ_SLOTS.with_borrow(|slots| {
        for slot in slots.values() {
            slot.callback.trace(tr);
        }
    });
}

/// Block until `ctrl.active`. Registers wakeup interest BEFORE re-checking
/// the flag (Notified::enable) so a concurrent resume can not be missed.
async fn wait_active(ctrl: &PumpControl) {
    if ctrl.active.load(Ordering::Acquire) {
        return;
    }

    let mut notified = std::pin::pin!(ctrl.wake.notified());
    loop {
        notified.as_mut().enable();
        if ctrl.active.load(Ordering::Acquire) {
            return;
        }
        notified.as_mut().await;
        notified.set(ctrl.wake.notified());
    }
}

/// Pump for an owned reader (Stdin — no locking).
async fn pump_direct(
    tx: Sender<PumpMsg>,
    fd: i32,
    ctrl: PumpControl,
    buf_size: usize,
    mut reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
) {
    let mut buf = vec![0u8; buf_size];
    loop {
        wait_active(&ctrl).await;
        match reader.read(&mut buf).await {
            Ok(0) => {
                let _ = tx.send(PumpMsg::Eof(fd));
                return;
            }
            Ok(n) => {
                let _ = tx.send(PumpMsg::Chunk(fd, buf[..n].to_vec()));
            }
            Err(e) => {
                let _ = tx.send(PumpMsg::Error(fd, e.to_string()));
                return;
            }
        }
    }
}

/// Pump for a lock-based reader (File, PipeRead). The Mutex is acquired once
/// per iteration so other tasks can interleave.
async fn pump_locked<M: tokio::io::AsyncRead + Unpin + Send + 'static>(
    tx: Sender<PumpMsg>,
    fd: i32,
    ctrl: PumpControl,
    mutex: Arc<tokio::sync::Mutex<M>>,
) {
    let mut buf = vec![0u8; BUF_SIZE_PIPE];
    loop {
        wait_active(&ctrl).await;
        let result = {
            let mut guard = mutex.lock().await;
            guard.read(&mut buf).await
        };
        match result {
            Ok(0) => {
                let _ = tx.send(PumpMsg::Eof(fd));
                return;
            }
            Ok(n) => {
                let _ = tx.send(PumpMsg::Chunk(fd, buf[..n].to_vec()));
            }
            Err(e) => {
                let _ = tx.send(PumpMsg::Error(fd, e.to_string()));
                return;
            }
        }
    }
}

/// Strip ONE trailing `\r` (CRLF handling); a lone `\r` inside the line stays.
fn strip_trailing_cr(bytes: &[u8]) -> &[u8] {
    match bytes.last() {
        Some(b'\r') => &bytes[..bytes.len() - 1],
        _ => bytes,
    }
}

/// One dispatched delivery, extracted under the state borrow and executed
/// outside it (callbacks may re-enter the state).
enum Delivery {
    Data(Object, Vec<u8>),
    Lines(Object, Vec<String>),
    Eof(Object, Option<String>, bool), // callback, flushed partial line, was_line_mode
    Error(Object, String),
}

fn call_cb(cx: &mut ScopedContext, cb: &Object, arg: JSValue) {
    // Clear any exception the callback may have thrown (ES §9.5)
    if cb.call(cx, JSValue::UNDEFINED, &[arg]).is_err() {
        let _e = cx.take_exception();
        #[cfg(debug_assertions)]
        eprintln!("[swallow-exc] class_io_handle.rs:{}: {:?}", line!(), _e);
    }
}

fn run_delivery(cx: &mut ScopedContext, d: Delivery) {
    match d {
        Delivery::Data(cb, data) => {
            if let Ok(arg) = TypedArray::create_from_vec_u8(cx, data).map(JSValue::from) {
                call_cb(cx, &cb, arg);
            }
        }
        Delivery::Lines(cb, lines) => {
            for line in lines {
                if let Ok(arg) = cx
                    .create_string_from_utf8(line.into_bytes())
                    .map(JSValue::make_string)
                {
                    call_cb(cx, &cb, arg);
                }
            }
        }
        Delivery::Eof(cb, partial, was_line) => {
            if was_line
                && let Some(text) = partial
                && let Ok(arg) = cx
                    .create_string_from_utf8(text.into_bytes())
                    .map(JSValue::make_string)
            {
                call_cb(cx, &cb, arg);
            }
            call_cb(cx, &cb, JSValue::NULL);
        }
        Delivery::Error(cb, err_msg) => {
            if let Some(err_obj) =
                cx.create_error_object(zuqe::ErrorKind::Internal, &err_msg, false)
            {
                call_cb(cx, &cb, err_obj.into());
            }
        }
    }
}

/// Poller microtask: converts raw pump chunks into mode-framed deliveries.
fn start_read_microtask(ctx: &mut ScopedContext) {
    if READ_POLLER_ACTIVE.swap(true, Ordering::AcqRel) {
        return; // a poller generation is already running
    }
    let rx = PUMP_CHANNEL.1.clone();

    crate::helpers::start_channel_poller(
        ctx,
        "io_handle",
        rx,
        |msg, cx| {
            let delivery = match msg {
                PumpMsg::Chunk(fd, data) => READ_SLOTS.with_borrow_mut(|slots| {
                    let slot = slots.get_mut(&fd)?;
                    Some(match slot.mode {
                        ReadMode::Data => Delivery::Data(slot.callback, data),
                        ReadMode::Line => {
                            slot.line_pending.extend_from_slice(&data);
                            let mut lines = Vec::new();
                            while let Some(pos) = slot.line_pending.iter().position(|&b| b == b'\n')
                            {
                                let line_bytes: Vec<u8> = slot.line_pending.drain(..=pos).collect();
                                let line_bytes = &line_bytes[..line_bytes.len() - 1];
                                let line_bytes = strip_trailing_cr(line_bytes);
                                if line_bytes.is_empty() || line_bytes.len() > MAX_LINE_BYTES {
                                    continue;
                                }
                                lines.push(String::from_utf8_lossy(line_bytes).into_owned());
                            }
                            Delivery::Lines(slot.callback, lines)
                        }
                    })
                }),
                PumpMsg::Eof(fd) => READ_SLOTS.with_borrow_mut(|slots| {
                    // Mark the pump dead so a later subscribe respawns it.
                    PUMPS.with_borrow(|pumps| {
                        if let Some(ctrl) = pumps.get(&fd) {
                            ctrl.dead.store(true, Ordering::Release);
                        }
                    });
                    let slot = slots.remove(&fd)?;
                    let partial = if slot.mode == ReadMode::Line && !slot.line_pending.is_empty() {
                        let stripped = strip_trailing_cr(&slot.line_pending);
                        if !stripped.is_empty() && stripped.len() <= MAX_LINE_BYTES {
                            Some(String::from_utf8_lossy(stripped).into_owned())
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let was_line = slot.mode == ReadMode::Line;
                    Some(Delivery::Eof(slot.callback, partial, was_line))
                }),
                PumpMsg::Error(fd, err_msg) => READ_SLOTS.with_borrow_mut(|slots| {
                    PUMPS.with_borrow(|pumps| {
                        if let Some(ctrl) = pumps.get(&fd) {
                            ctrl.dead.store(true, Ordering::Release);
                        }
                    });
                    let slot = slots.remove(&fd)?;
                    Some(Delivery::Error(slot.callback, err_msg))
                }),
            };
            if let Some(d) = delivery {
                run_delivery(cx, d);
            }
            true // always continue polling
        },
        || READ_SLOTS.with_borrow(|slots| !slots.is_empty()),
        || READ_POLLER_ACTIVE.store(false, Ordering::Release),
    );
}

fn get_io<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a IoPayload, i32> {
    JSStdioHandle::payload_of(val)
        .or_else(|| JSFileHandle::payload_from(val))
        .ok_or_else(|| ctx.throw_internal_error("invalid IoHandle instance"))
}

/// Shared implementation of onData / onLine — the mode is the only difference.
fn fh_subscribe(ctx: &mut ScopedContext, cc: CallContext, mode: ReadMode) -> JSResult {
    let this = cc.this();
    let desc = get_io(ctx, &this)?;
    let raw_fd = desc.as_virt_fd();

    // Cannot read from stdout/stderr
    if matches!(desc, IoPayload::Stdout | IoPayload::Stderr) {
        return Err(ctx.throw_type_error("cannot read from stdout/stderr"));
    }

    let arg = cc.arg(0);
    if arg.is_null() {
        // Unsubscribe: drop the slot and pause the pump so nothing is
        // consumed while nobody listens.
        READ_SLOTS.with_borrow_mut(|slots| {
            if slots.remove(&raw_fd).is_none() {
                return;
            }
            PUMPS.with_borrow(|pumps| {
                if let Some(ctrl) = pumps.get(&raw_fd)
                    && !ctrl.dead.load(Ordering::Acquire)
                {
                    ctrl.active.store(false, Ordering::Release);
                    ctrl.wake.notify_one();
                }
            });
        });
        return Ok(JSValue::UNDEFINED);
    }
    let callback = arg.ensure_callable(ctx)?;

    // Conflict / replace check. First registration wins: a later conflicting
    // mode is ignored with a warning (not an error), same mode just replaces
    // the callback.
    enum SubAction {
        Replace,
        WarnIgnore,
        Subscribe,
    }
    let action = READ_SLOTS.with_borrow_mut(|slots| {
        match slots.get_mut(&raw_fd) {
            Some(slot) if slot.mode != mode => SubAction::WarnIgnore,
            Some(slot) => {
                slot.callback = callback;
                SubAction::Replace
            }
            None => SubAction::Subscribe,
        }
    });

    match action {
        SubAction::Replace => return Ok(JSValue::UNDEFINED),
        SubAction::WarnIgnore => {
            let existing = READ_SLOTS.with_borrow(|slots| {
                slots.get(&raw_fd).map(|slot| slot.mode.name()).unwrap_or("?")
            });
            eprintln!(
                "[aike] warning: {}() ignored — this handle already has an active {} listener (first registration wins). Call {}(null) first to switch modes.",
                mode.name(),
                existing,
                existing,
            );
            return Ok(JSValue::UNDEFINED);
        }
        SubAction::Subscribe => {}
    }

    // Fresh subscription: insert the slot and ensure a live pump.
    let (ctrl, spawn_pump) = {
        READ_SLOTS.with_borrow_mut(|slots| {
            slots.insert(
                raw_fd,
                ReadSlot {
                    mode,
                    callback,
                    line_pending: Vec::new(),
                },
            );
        });
        let (ctrl, needs_spawn) = PUMPS.with_borrow_mut(|pumps| match pumps.get(&raw_fd) {
            // Live pump: reuse and resume.
            Some(c) if !c.dead.load(Ordering::Acquire) => {
                c.active.store(true, Ordering::Release);
                c.wake.notify_one();
                (c.clone(), false)
            }
            // Dead pump (stream ended earlier): reset and spawn a new one.
            Some(c) => {
                c.dead.store(false, Ordering::Release);
                c.active.store(true, Ordering::Release);
                (c.clone(), true)
            }
            None => {
                let c = PumpControl::new();
                pumps.insert(raw_fd, c.clone());
                (c, true)
            }
        });
        (ctrl, needs_spawn)
    };

    if spawn_pump {
        let tx = PUMP_CHANNEL.0.clone();
        match desc {
            IoPayload::Stdin => {
                tokio::spawn(pump_direct(
                    tx,
                    raw_fd,
                    ctrl,
                    desc.read_buf_size(),
                    tokio::io::stdin(),
                ));
            }
            IoPayload::File(fd, _) => {
                tokio::spawn(pump_locked(tx, raw_fd, ctrl, fd.clone()));
            }
            IoPayload::PipeRead(reader, _) => {
                tokio::spawn(pump_locked(tx, raw_fd, ctrl, reader.clone()));
            }
            IoPayload::PipeWrite(..) => {
                let _ = tx.send(PumpMsg::Error(
                    raw_fd,
                    "cannot read from pipe writer".to_string(),
                ));
            }
            _ => unreachable!(),
        }
    }

    // Ensure a read-poller generation is running (no-op if one already is).
    start_read_microtask(ctx);

    Ok(JSValue::UNDEFINED)
}

/// onData(callback) — raw chunk delivery (Uint8Array per read), `null` at EOF.
fn js_fh_onData(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    fh_subscribe(ctx, cc, ReadMode::Data)
}

/// onLine(callback) — newline-delimited text framing; see module docs above.
fn js_fh_onLine(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    fh_subscribe(ctx, cc, ReadMode::Line)
}

/// text() — async read as string via the real fd
fn js_fh_text(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let this = cc.this();
    let desc = get_io(ctx, &this)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(ctx, |cx, payload| -> JSResult {
            let text = payload.map_err(|e| cx.throw_internal_error(&e))?;
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::make_string)
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
        IoPayload::File(fd, _) => {
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
    let desc = get_io(ctx, &this)?;

    let (promise, tx) = Promise::create_with_channel::<Result<serde_json::Value, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;
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
        IoPayload::File(fd, _) => {
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
    let desc = get_io(ctx, &this)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| cx.throw_internal_error(&e))?;
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
        IoPayload::File(fd, _) => {
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
    let desc = get_io(ctx, &this)?;

    let (promise, tx) = Promise::create_with_channel::<Result<std::fs::Metadata, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let meta = payload.map_err(|e| cx.throw_internal_error(&e))?;
            make_stats(cx, &meta)
        },
    )?;

    match desc {
        IoPayload::Stdin | IoPayload::Stdout | IoPayload::Stderr => {
            // std streams: stat via /dev/stdin etc. is unreliable, return zero-stats
            let _ = tx.send(Err("cannot stat standard stream".to_string()));
        }
        IoPayload::File(fd, _) => {
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
    let desc = get_io(ctx, &this)?;

    // Optional length arg; 0 = read to EOF
    let max_len: usize = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(ctx)?.to_i32().max(0) as usize
    } else {
        0
    };

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| cx.throw_internal_error(&e))?;
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
        IoPayload::File(fd, _) => {
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

/// write() — sync for stdout/stderr (per Node.js standard), async for files/pipes
fn js_fh_writeAll(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_io(ctx, &this)?;

    let arg = cc.arg(0);

    // Extract bytes: TypedArray, ArrayBuffer or String
    let data: Vec<u8> = if let Some(s) = arg.as_string(true) {
        s.content().encode_utf8(false).into_owned().into_bytes()
    } else if let Some(p) = arg.payload() {
        if let Some(ta) = p.typed_array() {
            ta.bytes().to_vec()
        } else if let Some(ab) = p.array_buffer() {
            ab.bytes().to_vec()
        } else {
            return Err(ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument"));
        }
    } else {
        return Err(ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument"));
    };

    match desc {
        IoPayload::Stdin => Err(ctx.throw_type_error("cannot write to stdin")),
        IoPayload::Stdout => {
            let mut out = std::io::stdout().lock();
            let ok = out.write_all(&data).is_ok();
            let _ = out.flush();
            Ok(JSValue::from(ok))
        }
        IoPayload::Stderr => {
            let mut out = std::io::stderr().lock();
            let ok = out.write_all(&data).is_ok();
            let _ = out.flush();
            Ok(JSValue::from(ok))
        }
        IoPayload::PipeWrite(fd) => {
            // Pipe write(2) is a synchronous memcpy to kernel buffer
            let ok = (|| -> std::io::Result<()> {
                let mut offset = 0;
                while offset < data.len() {
                    // SAFETY: fd is a valid pipe fd owned by this IoHandle.
                    let n = unsafe {
                        libc::write(
                            fd.as_virt_fd() as _,
                            data[offset..].as_ptr() as *const _,
                            data[offset..].len() as _,
                        )
                    };
                    if n < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    offset += n as usize;
                }
                Ok(())
            })()
            .is_ok();
            Ok(JSValue::from(ok))
        }
        IoPayload::PipeRead(..) => Err(ctx.throw_type_error("cannot write to pipe reader")),
        // async
        IoPayload::File(fd, _) => {
            let (promise, tx) = Promise::create_with_channel::<Result<(), String>>(
                ctx,
                |cx, payload| -> JSResult {
                    payload.map_err(|e| cx.throw_internal_error(&e))?;
                    Ok(JSValue::UNDEFINED)
                },
            )?;
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;
                let result = file.write_all(&data).await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
            Ok(promise.into())
        }
    }
}

/// flush() — sync for stdout/stderr, async for files/pipes
fn js_fh_flush(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_io(ctx, &this)?;

    match desc {
        IoPayload::Stdin => Ok(JSValue::UNDEFINED),
        IoPayload::Stdout => {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            Ok(JSValue::UNDEFINED)
        }
        IoPayload::Stderr => {
            use std::io::Write;
            let _ = std::io::stderr().flush();
            Ok(JSValue::UNDEFINED)
        }
        IoPayload::PipeWrite(..) | IoPayload::PipeRead(..) => {
            // Pipe writes go directly to kernel pipe buffer —
            // no userspace buffering to flush.
            Ok(JSValue::UNDEFINED)
        }
        // async
        IoPayload::File(fd, _) => {
            use tokio::io::AsyncWriteExt;
            let (promise, tx) = Promise::create_with_channel::<Result<(), String>>(
                ctx,
                |cx, payload| -> JSResult {
                    payload.map_err(|e| cx.throw_internal_error(&e))?;
                    Ok(JSValue::UNDEFINED)
                },
            )?;
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;
                let result = file.flush().await.map_err(|e| e.to_string());
                let _ = PollSender::from(tx).send_async(result).await;
            });
            Ok(promise.into())
        }
    }
}

/// close() — drop the io
fn js_fh_close(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut val = cc.this();
    // Detach whichever concrete handle type this is (both share IoPayload).
    if JSStdioHandle::take_payload_of(&mut val).is_none() {
        JSFileHandle::take_payload_of(&mut val);
    }
    Ok(JSValue::UNDEFINED)
}

/// stat helper — shared between IoHandle.stat and fs.stat
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
        JSValue::from(meta.is_file()),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isDirectory",
        JSValue::from(meta.is_dir()),
        zuqe::JS_PROP_ENUMERABLE,
    )?;
    obj.define_property_value_str(
        ctx,
        "isSymbolicLink",
        JSValue::from(meta.is_symlink()),
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
    let desc = get_io(ctx, &this)?;
    Ok(JSValue::from(desc.as_virt_fd()))
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
    let desc = get_io(ctx, &this)?;

    if matches!(desc, IoPayload::Stdout | IoPayload::Stderr) {
        return Err(ctx.throw_type_error("cannot iterate stdout/stderr"));
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
        IoPayload::File(fd, _) => {
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
                FileChunk::Error(e) => Err(cx.throw_internal_error(&e)),
            }
        },
    )?;

    Ok(iter.into())
}

// ---------------------------------------------------------------------------
// StdioHandle -- sync write/flush for stdin/stdout/stderr/pipeWrite
// ---------------------------------------------------------------------------

fn js_fh_writeSync(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_io(ctx, &this)?;

    let arg = cc.arg(0);
    let data: Vec<u8> = if let Some(s) = arg.as_string(true) {
        s.content().encode_utf8(false).into_owned().into_bytes()
    } else if let Some(p) = arg.payload() {
        if let Some(ta) = p.typed_array() {
            ta.bytes().to_vec()
        } else if let Some(ab) = p.array_buffer() {
            ab.bytes().to_vec()
        } else {
            return Err(ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument"));
        }
    } else {
        return Err(ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument"));
    };

    match desc {
        IoPayload::Stdin => Err(ctx.throw_type_error("cannot write to stdin")),
        IoPayload::Stdout => {
            let mut out = std::io::stdout().lock();
            let ok = out.write_all(&data).is_ok();
            let _ = out.flush();
            Ok(JSValue::from(ok))
        }
        IoPayload::Stderr => {
            let mut out = std::io::stderr().lock();
            let ok = out.write_all(&data).is_ok();
            let _ = out.flush();
            Ok(JSValue::from(ok))
        }
        IoPayload::PipeWrite(fd) => {
            let ok = (|| -> std::io::Result<()> {
                let mut offset = 0;
                while offset < data.len() {
                    let n = unsafe {
                        libc::write(
                            fd.as_virt_fd() as _,
                            data[offset..].as_ptr() as *const _,
                            data[offset..].len() as _,
                        )
                    };
                    if n < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    offset += n as usize;
                }
                Ok(())
            })()
            .is_ok();
            Ok(JSValue::from(ok))
        }
        _ => Err(ctx.throw_type_error("cannot write to this handle")),
    }
}

fn js_fh_flushSync(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let val = cc.this();
    let desc = get_io(ctx, &val)?;
    match desc {
        IoPayload::PipeWrite(..) => {}
        IoPayload::Stdout => {
            let _ = std::io::stdout().flush();
        }
        IoPayload::Stderr => {
            let _ = std::io::stderr().flush();
        }
        IoPayload::Stdin => {}
        _ => {
            return Err(ctx.throw_type_error("cannot flush this handle"));
        }
    }
    Ok(JSValue::UNDEFINED)
}

const STDIO_HANDLE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("fd", Some(js_fh_fd), None, PROP_FD as _),
    FieldDef::FUNC("writeSync", 1, js_fh_writeSync, 0),
    FieldDef::FUNC("flush", 0, js_fh_flushSync, 0),
    FieldDef::FUNC("close", 0, js_fh_close, 0),
    FieldDef::FUNC("text", 0, js_fh_text, 0),
    FieldDef::FUNC("json", 0, js_fh_json, 0),
    FieldDef::FUNC("bytes", 0, js_fh_bytes, 0),
    FieldDef::FUNC("read", 1, js_fh_read, 0),
    FieldDef::FUNC("stat", 0, js_fh_stat, 0),
    FieldDef::FUNC("onData", 1, js_fh_onData, 0),
    FieldDef::FUNC("onLine", 1, js_fh_onLine, 0),
    FieldDef::FUNC("[Symbol.asyncIterator]", 0, js_fh_asyncIterator, 0),
];

def_native_object!(
    "StdioHandle",
    JSStdioHandle,
    IoPayload,
    STDIO_HANDLE_PROTO,
    None
);

impl JSStdioHandle {
    pub fn new_instance_stdin(ctx: &mut ScopedContext) -> Result<Self, i32> {
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::Stdin)
    }

    pub fn new_instance_stdout(ctx: &mut ScopedContext) -> Result<Self, i32> {
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::Stdout)
    }

    pub fn new_instance_stderr(ctx: &mut ScopedContext) -> Result<Self, i32> {
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::Stderr)
    }

    pub fn new_instance_pipe_write(
        ctx: &mut ScopedContext,
        #[cfg(unix)] writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static + AsRawFd,
        #[cfg(not(unix))] _writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
        #[allow(unused)] fd: i32,
    ) -> Result<Self, i32> {
        #[cfg(unix)]
        let pipe_fd = {
            let raw_fd = writer.as_raw_fd();
            std::mem::forget(writer);
            PipeFd(raw_fd)
        };
        #[cfg(not(unix))]
        let pipe_fd = PipeFd(fd);
        Self::new_instance(ctx, JSValue::UNDEFINED, IoPayload::PipeWrite(pipe_fd))
    }

    pub fn new_instance_pipe_read(
        ctx: &mut ScopedContext,
        reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        fd: i32,
    ) -> Result<Self, i32> {
        Self::new_instance(
            ctx,
            JSValue::UNDEFINED,
            IoPayload::PipeRead(Arc::new(tokio::sync::Mutex::new(Box::new(reader))), fd),
        )
    }
}

// ---------------------------------------------------------------------------
// FileHandle -- async write/flush for file/pipeRead
// ---------------------------------------------------------------------------

const FILE_HANDLE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("fd", Some(js_fh_fd), None, PROP_FD as _),
    FieldDef::FUNC("write", 1, js_fh_writeAll, 0),
    FieldDef::FUNC("flush", 0, js_fh_flush, 0),
    FieldDef::FUNC("close", 0, js_fh_close, 0),
    FieldDef::FUNC("text", 0, js_fh_text, 0),
    FieldDef::FUNC("json", 0, js_fh_json, 0),
    FieldDef::FUNC("bytes", 0, js_fh_bytes, 0),
    FieldDef::FUNC("read", 1, js_fh_read, 0),
    FieldDef::FUNC("stat", 0, js_fh_stat, 0),
    FieldDef::FUNC("onData", 1, js_fh_onData, 0),
    FieldDef::FUNC("onLine", 1, js_fh_onLine, 0),
    FieldDef::FUNC("[Symbol.asyncIterator]", 0, js_fh_asyncIterator, 0),
];

def_native_object!(
    "FileHandle",
    JSFileHandle,
    IoPayload,
    FILE_HANDLE_PROTO,
    Some(ClassCtorDef::new(
        |ctx, _| { Err(ctx.throw_internal_error("can't instantiate with new")) },
        0,
        false
    ))
);

impl JSFileHandle {
    pub fn new_instance_file(
        ctx: &mut ScopedContext,
        new_target: JSValue,
        file: tokio::fs::File,
    ) -> Result<Self, i32> {
        Self::new_instance(
            ctx,
            new_target,
            IoPayload::File(Arc::new(tokio::sync::Mutex::new(file)), alloc_fd()),
        )
    }

    pub fn new_instance_pipe_read(
        ctx: &mut ScopedContext,
        reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        fd: i32,
    ) -> Result<Self, i32> {
        Self::new_instance(
            ctx,
            JSValue::UNDEFINED,
            IoPayload::PipeRead(Arc::new(tokio::sync::Mutex::new(Box::new(reader))), fd),
        )
    }

    fn payload_from(val: &JSValue) -> Option<&IoPayload> {
        JSFileHandle::payload_of(val)
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_stdio_handle(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSStdioHandle::register(rt)
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_file_handle(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSFileHandle::register(rt)
}
