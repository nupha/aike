// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    cell::RefCell,
    collections::HashMap,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zuqe::{
    CallContext, ClassCtorDef, FieldDef, JSResult, JSValue, Object, ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise, typed_array::TypedArray},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use super::AikeRuntimeState;

/// Write half of a child pipe.
///
/// Unix owns a raw fd (closed on drop). Windows owns a raw pipe `HANDLE`; the
/// writer it came from is leaked by `new_instance_pipe_write` so the handle
/// stays valid (same lifetime trick as the Unix fd). Windows must NOT go
/// through `libc::write`: an invalid/`-1` CRT fd trips ucrt's invalid-parameter
/// handler, which `__fastfail`s the process (exit 0xC0000409). `WriteFile` is
/// used instead.
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

    /// Blocking write-all; `false` on error (mirrors `write(2)` short writes).
    fn write_all(&self, data: &[u8]) -> bool {
        let mut offset = 0;
        while offset < data.len() {
            let n = unsafe {
                libc::write(
                    self.0,
                    data[offset..].as_ptr() as *const _,
                    data[offset..].len() as _,
                )
            };
            if n < 0 {
                return false;
            }
            offset += n as usize;
        }
        true
    }
}

/// Stored as `isize` (not `RawHandle`) so the owning `IoPayload` stays `Send`.
#[cfg(windows)]
pub(crate) struct PipeFd(isize);

// Closing the write HANDLE is what delivers EOF to the child (mirrors
// `libc::close` on Unix); the leaked writer never touches it again.
#[cfg(windows)]
impl Drop for PipeFd {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0 as *mut _);
            }
        }
    }
}

#[cfg(windows)]
impl PipeFd {
    fn as_virt_fd(&self) -> i32 {
        -1
    }

    /// Blocking write-all via `WriteFile` on the pipe `HANDLE`.
    fn write_all(&self, data: &[u8]) -> bool {
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = &data[offset..];
            let len = chunk.len().min(u32::MAX as usize) as u32;
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    self.0 as *mut _,
                    chunk.as_ptr() as *const _,
                    len,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                return false;
            }
            offset += written as usize;
        }
        true
    }
}

/// Fallback for platforms that are neither Unix nor Windows.
#[cfg(not(any(unix, windows)))]
pub(crate) struct PipeFd(i32);

#[cfg(not(any(unix, windows)))]
impl PipeFd {
    fn as_virt_fd(&self) -> i32 {
        self.0
    }

    fn write_all(&self, _data: &[u8]) -> bool {
        false
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

impl GcTrace for IoPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
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

/// Per-runtime read-subscription state, stored on `AikeRuntimeState`.
/// `PUMPS` stays thread-global: it is keyed by fd (an OS resource), not by
/// runtime, and must persist across poller generations for a handle.
#[derive(Default)]
pub(crate) struct IoState {
    /// Active subscriptions (at most one per fd).
    read_slots: HashMap<i32, ReadSlot>,
    /// This runtime's pump channel. The poller may stop and restart freely;
    /// pump tasks keep sending into this receiver regardless of generation.
    pump_channel: Option<(zuqe::WakeSender<PumpMsg>, zuqe::WakeReceiver<PumpMsg>)>,
    /// Whether a read-poller generation is currently queued/running.
    read_poller_active: bool,
}

impl GcTrace for IoState {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        for slot in self.read_slots.values() {
            slot.callback.trace(gcx);
        }
    }
}

thread_local! {
    /// Pump controls — persist across poller generations so a handle never
    /// accumulates a second concurrent reader. Keyed by fd (a process resource).
    static PUMPS: RefCell<HashMap<i32, PumpControl>> = RefCell::new(HashMap::new());
}

fn pump_channel(
    ctx: &mut ScopedContext,
) -> (zuqe::WakeSender<PumpMsg>, zuqe::WakeReceiver<PumpMsg>) {
    if AikeRuntimeState::of(ctx).io.pump_channel.is_none() {
        let ch = ctx.make_channel::<PumpMsg>(256);
        AikeRuntimeState::of(ctx).io.pump_channel = Some(ch);
    }
    AikeRuntimeState::of(ctx).io.pump_channel.clone().unwrap()
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
    tx: zuqe::WakeSender<PumpMsg>,
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
    tx: zuqe::WakeSender<PumpMsg>,
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
        let exc = cx.take_exception();
        #[cfg(debug_assertions)]
        crate::helpers::print_exc(cx, exc);
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
    if AikeRuntimeState::of(ctx).io.read_poller_active {
        return; // a poller generation for this runtime is already running
    }
    AikeRuntimeState::of(ctx).io.read_poller_active = true;
    let rx = pump_channel(ctx).1;

    crate::helpers::start_channel_poller(
        ctx,
        "io_handle",
        rx,
        |msg, cx| {
            let delivery = match msg {
                PumpMsg::Chunk(fd, data) => {
                    let slots = &mut AikeRuntimeState::of(cx).io.read_slots;
                    match slots.get_mut(&fd) {
                        None => None,
                        Some(slot) => Some(match slot.mode {
                            ReadMode::Data => Delivery::Data(slot.callback, data),
                            ReadMode::Line => {
                                slot.line_pending.extend_from_slice(&data);
                                let mut lines = Vec::new();
                                while let Some(pos) =
                                    slot.line_pending.iter().position(|&b| b == b'\n')
                                {
                                    let line_bytes: Vec<u8> =
                                        slot.line_pending.drain(..=pos).collect();
                                    let line_bytes = &line_bytes[..line_bytes.len() - 1];
                                    let line_bytes = strip_trailing_cr(line_bytes);
                                    if line_bytes.is_empty() || line_bytes.len() > MAX_LINE_BYTES {
                                        continue;
                                    }
                                    lines.push(String::from_utf8_lossy(line_bytes).into_owned());
                                }
                                Delivery::Lines(slot.callback, lines)
                            }
                        }),
                    }
                }
                PumpMsg::Eof(fd) => {
                    // Mark the pump dead so a later subscribe respawns it.
                    PUMPS.with_borrow(|pumps| {
                        if let Some(ctrl) = pumps.get(&fd) {
                            ctrl.dead.store(true, Ordering::Release);
                        }
                    });
                    AikeRuntimeState::of(cx)
                        .io
                        .read_slots
                        .remove(&fd)
                        .map(|slot| {
                            let partial =
                                if slot.mode == ReadMode::Line && !slot.line_pending.is_empty() {
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
                            Delivery::Eof(slot.callback, partial, was_line)
                        })
                }
                PumpMsg::Error(fd, err_msg) => {
                    PUMPS.with_borrow(|pumps| {
                        if let Some(ctrl) = pumps.get(&fd) {
                            ctrl.dead.store(true, Ordering::Release);
                        }
                    });
                    AikeRuntimeState::of(cx)
                        .io
                        .read_slots
                        .remove(&fd)
                        .map(|slot| Delivery::Error(slot.callback, err_msg))
                }
            };
            if let Some(d) = delivery {
                run_delivery(cx, d);
            }
            true // always continue polling
        },
        |cx| !AikeRuntimeState::of(cx).io.read_slots.is_empty(),
        |cx| {
            AikeRuntimeState::of(cx).io.read_poller_active = false;
        },
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
        let removed = AikeRuntimeState::of(ctx)
            .io
            .read_slots
            .remove(&raw_fd)
            .is_some();
        if removed {
            PUMPS.with_borrow(|pumps| {
                if let Some(ctrl) = pumps.get(&raw_fd)
                    && !ctrl.dead.load(Ordering::Acquire)
                {
                    ctrl.active.store(false, Ordering::Release);
                    ctrl.wake.notify_one();
                }
            });
        }
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
    let action = {
        let slots = &mut AikeRuntimeState::of(ctx).io.read_slots;
        match slots.get_mut(&raw_fd) {
            Some(slot) if slot.mode != mode => SubAction::WarnIgnore,
            Some(slot) => {
                slot.callback = callback;
                SubAction::Replace
            }
            None => SubAction::Subscribe,
        }
    };

    match action {
        SubAction::Replace => return Ok(JSValue::UNDEFINED),
        SubAction::WarnIgnore => {
            let existing = AikeRuntimeState::of(ctx)
                .io
                .read_slots
                .get(&raw_fd)
                .map(|slot| slot.mode.name())
                .unwrap_or("?");
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
    AikeRuntimeState::of(ctx).io.read_slots.insert(
        raw_fd,
        ReadSlot {
            mode,
            callback,
            line_pending: Vec::new(),
        },
    );
    let (ctrl, spawn_pump) = {
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
        let tx = pump_channel(ctx).0;
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

// ---- shared read plumbing for fh.text()/json()/bytes()/read() ----

/// Read a reader to end. Text mode validates UTF-8 via `read_to_string`,
/// keeping the exact io-error wording of the original per-arm implementations.
async fn read_to_end<R>(mut r: R, as_text: bool) -> Result<Vec<u8>, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    if as_text {
        let mut s = String::new();
        r.read_to_string(&mut s).await.map(|_| s.into_bytes())
    } else {
        let mut v = Vec::new();
        r.read_to_end(&mut v).await.map(|_| v)
    }
    .map_err(|e| e.to_string())
}

/// Read up to `max_len` bytes from the current position (0 = to EOF).
async fn read_capped<R>(mut r: R, max_len: usize) -> Result<Vec<u8>, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    if max_len == 0 {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).await.map(|_| buf)
    } else {
        let mut buf = vec![0u8; max_len];
        r.read(&mut buf).await.map(|n| {
            buf.truncate(n);
            buf
        })
    }
    .map_err(|e| e.to_string())
}

/// Spawn the 5-arm dispatch shared by text()/json()/bytes(): whole-stream read
/// (file handles seek to position 0 first), then convert and deliver.
fn spawn_whole_read<T, F>(
    desc: &IoPayload,
    tx: zuqe::WakeSender<Result<T, String>>,
    as_text: bool,
    convert: F,
) where
    T: Send + Unpin + 'static,
    F: FnOnce(Vec<u8>) -> Result<T, String> + Send + 'static,
{
    // Runs on the JS thread; captured here for the spawned tokio tasks below.
    match desc {
        IoPayload::Stdin => {
            tokio::spawn(async move {
                let result = read_to_end(tokio::io::stdin(), as_text)
                    .await
                    .and_then(convert);
                let _ = tx.send_async(result).await;
            });
        }
        IoPayload::Stdout | IoPayload::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        IoPayload::File(fd, _) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncSeekExt;
                let mut file = fd.lock().await;
                let _ = file.seek(std::io::SeekFrom::Start(0)).await;
                let result = read_to_end(&mut *file, as_text).await.and_then(convert);
                let _ = tx.send_async(result).await;
            });
        }
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut r = reader.lock().await;
                let result = read_to_end(&mut *r, as_text).await.and_then(convert);
                let _ = tx.send_async(result).await;
            });
        }
        IoPayload::PipeWrite(..) => {
            let _ = tx.send(Err("cannot read from pipe writer".to_string()));
        }
    }
}

/// text() — async read as string via the real fd
fn js_fh_text(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let desc = get_io(ctx, &this)?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| cx.throw_internal_error(&e))?;
            cx.create_string_from_utf8(bytes).map(JSValue::make_string)
        })?;

    spawn_whole_read(desc, tx, true, Ok);

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

    spawn_whole_read(desc, tx, true, |bytes| {
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    });

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

    spawn_whole_read(desc, tx, false, Ok);

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
                let _ = tx.send_async(result).await;
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

    match desc {
        IoPayload::Stdin => {
            tokio::spawn(async move {
                let result = read_capped(tokio::io::stdin(), max_len).await;
                let _ = tx.send_async(result).await;
            });
        }
        IoPayload::Stdout | IoPayload::Stderr => {
            let _ = tx.send(Err("cannot read from stdout/stderr".to_string()));
        }
        IoPayload::File(fd, _) => {
            let fd = fd.clone();
            tokio::spawn(async move {
                let mut file = fd.lock().await;
                let result = read_capped(&mut *file, max_len).await;
                let _ = tx.send_async(result).await;
            });
        }
        IoPayload::PipeRead(reader, _) => {
            let reader = reader.clone();
            tokio::spawn(async move {
                let mut r = reader.lock().await;
                let result = read_capped(&mut *r, max_len).await;
                let _ = tx.send_async(result).await;
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
    } else {
        if let Some(p) = arg.as_object() {
            if let Some(ta) = p.payload_typed_array() {
                ta.bytes().to_vec()
            } else if let Some(ab) = p.payload_array_buffer() {
                ab.bytes().to_vec()
            } else {
                return Err(
                    ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument")
                );
            }
        } else {
            return Err(ctx.throw_type_error("expect string, TypedArray or ArrayBuffer argument"));
        }
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
            let ok = fd.write_all(&data);
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
                let _ = tx.send_async(result).await;
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
                let _ = tx.send_async(result).await;
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
    let (tx, rx) = ctx.make_channel::<FileChunk>(8);

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
    } else if let Some(o) = arg.as_object() {
        if let Some(ta) = o.payload_typed_array() {
            ta.bytes().to_vec()
        } else if let Some(ab) = o.payload_array_buffer() {
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
            let ok = fd.write_all(&data);
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
        #[cfg(windows)] writer: impl tokio::io::AsyncWrite
        + Unpin
        + Send
        + 'static
        + std::os::windows::io::AsRawHandle,
        #[cfg(not(any(unix, windows)))] _writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
        #[allow(unused)] fd: i32,
    ) -> Result<Self, i32> {
        // Leak the writer so its fd/HANDLE stays valid for the handle's whole
        // lifetime; the payload keeps only the raw descriptor.
        #[cfg(unix)]
        let pipe_fd = {
            let raw_fd = writer.as_raw_fd();
            std::mem::forget(writer);
            PipeFd(raw_fd)
        };
        #[cfg(windows)]
        let pipe_fd = {
            let handle = writer.as_raw_handle() as isize;
            std::mem::forget(writer);
            PipeFd(handle)
        };
        #[cfg(not(any(unix, windows)))]
        let pipe_fd = {
            let _ = _writer;
            PipeFd(fd)
        };
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
