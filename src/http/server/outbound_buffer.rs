// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Shared write-side component for WebSocket (`WSStream`) and SSE
//! (`HttpResponseSse`).
//!
//! Both share the *exact same* "push one frame to the client" semantics:
//! a bounded `mpsc::Sender<String>` plus "channel-full → return a Promise that
//! resolves when capacity frees up" backpressure. This module is the single
//! home for that logic so `ws.rs` and `response.rs` do not each re-implement it.
//!
//! What is intentionally NOT shared (kept asymmetric):
//! - read/pull side (async iterator) — WS only; SSE is server→client only.
//! - class identity — `WebSocketStream` vs `HttpResponseSse` are distinct classes.
//! - bridge task — WS only; SSE is consumed directly by axum's `Sse`.
//! - pub/sub (`subscribe`/`publish`) — WS only.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use zuqe::{
    JSValue, Object, ScopedContext,
    builtins::promise::Promise,
    gc_lite::{GcTrace, GcTraceCtx},
};

use super::class_request;

/// Bounded channel capacity for the outbound (server→client) frame buffer.
pub(crate) const OUTBOUND_CAP: usize = 64;

/// Write-side backpressure buffer shared by WS and SSE.
///
/// Holds a bounded `mpsc::Sender<String>` (the frames JS `write()` pushes) and
/// a shared queue of frames that could not be sent yet because the channel was
/// full. When the channel fills, `js_stream_write` returns a `Promise` whose
/// resolver is parked in `pending`; a spawned tokio watcher waits for capacity
/// and flushes the backlog, resolving each parked Promise in turn.
pub(crate) struct OutboundBuffer {
    /// Bounded sender. `None` once the stream is explicitly closed.
    pub(crate) tx: Option<tokio::sync::mpsc::Sender<String>>,
    /// Frames awaiting capacity + the resolver that completes their Promise.
    pending: Arc<Mutex<VecDeque<(zuqe::WakeSender<()>, String)>>>,
    /// Backpressure-relief callback (Bun `drain` semantics), invoked on the JS
    /// thread when the backlog drains.
    pub(crate) on_drain: Option<Object>,
    /// True while a backlog exists; used to fire `on_drain` exactly once when
    /// the backlog transitions back to empty (after the watcher drains it).
    drain_notified: bool,
    /// Shared close-callback list — fired on first channel-close (client disconnect).
    close_cbs: Option<class_request::CloseCbs>,
    /// Signals the disconnect watcher task when this buffer is shut down or
    /// dropped. The task also awaits the strong sender's `closed()` (receiver
    /// side), so between the two every way a stream ends is observed without
    /// the task pinning the channel open — see [`Self::arm_disconnect_watcher`].
    close_notify: Arc<tokio::sync::Notify>,
}

impl Drop for OutboundBuffer {
    fn drop(&mut self) {
        // GC dropped the last JS reference: release the watcher, which drops
        // the strong sender it holds so the receiver can end.
        self.close_notify.notify_one();
    }
}

impl OutboundBuffer {
    /// Create a new buffer together with the `Receiver` the consumer
    /// (bridge task or axum `Sse`) reads frames from.
    pub(crate) fn new(cap: usize) -> (Self, tokio::sync::mpsc::Receiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        (
            Self {
                tx: Some(tx),
                pending: Arc::new(Mutex::new(VecDeque::new())),
                on_drain: None,
                drain_notified: false,
                close_cbs: None,
                close_notify: Arc::new(tokio::sync::Notify::new()),
            },
            rx,
        )
    }

    /// Wrap a sender that was created earlier (e.g. at WS upgrade time).
    pub(crate) fn from_existing(tx: tokio::sync::mpsc::Sender<String>) -> Self {
        Self {
            tx: Some(tx),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            on_drain: None,
            drain_notified: false,
            close_cbs: None,
            close_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Attach a shared close-callback list. Fired on first channel-close.
    pub(crate) fn set_close_cbs(&mut self, cbs: class_request::CloseCbs) {
        self.close_cbs = Some(cbs);
    }

    /// Arm a disconnect watcher. Returns a `WakeReceiver<()>` that receives a
    /// signal when the stream ends either way — poll it via `queue_job` on the
    /// JS thread to fire `close_cbs` (otherwise that job stays `Block` forever
    /// and the event loop never idles: the process would not exit).
    ///
    /// Both ends must be watched, and the task therefore holds a STRONG sender
    /// for `closed()`:
    ///
    /// - receiver dropped (client disconnected / axum dropped the body) →
    ///   `closed()` resolves. Watching only the sender missed this — which is
    ///   what the old `weak.upgrade()` poll did, leaving the job blocked and
    ///   the process alive after an aborted SSE stream;
    /// - JS side gone (`sse.close()` / GC of the buffer) → `close_notify`
    ///   (fired by `shutdown()` and `Drop`), after which the task drops its own
    ///   sender clone so the receiver can really end.
    pub(crate) fn arm_disconnect_watcher(&mut self, ctx: &ScopedContext) -> zuqe::WakeReceiver<()> {
        let (wake_tx, wake_rx) = ctx.make_channel::<()>(1);
        let tx = self.tx.as_ref().expect("sender armed").clone();
        let notify = self.close_notify.clone();

        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = tx.closed() => {}                 // receiver gone
                _ = notify.notified() => {}           // sender side gone
            }
            let _ = wake_tx.send(());
            drop(tx); // release the channel if only we still held it
        });

        wake_rx
    }

    /// Drop the sender so the underlying stream ends (SSE close / WS close).
    pub(crate) fn shutdown(&mut self) {
        self.tx.take();
        // Wake the watcher: our strong sender was the one keeping its
        // `closed()` pending (it holds a clone).
        self.close_notify.notify_one();
    }

    /// Push one frame; on channel-full, enqueue a parked Promise and arm the
    /// drain watcher. Returns `Some(promise)` when the caller must await drain.
    fn send(&mut self, ctx: &mut ScopedContext, frame: String) -> Result<Option<JSValue>, i32> {
        // If a backlog already exists, the channel was full — queue this too.
        {
            let q = self.pending.lock().unwrap();
            if !q.is_empty() {
                let (p, slot) =
                    Promise::create_with_channel::<()>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;
                drop(q);
                self.pending.lock().unwrap().push_back((slot, frame));
                return Ok(Some(p.into()));
            }
        }

        match &self.tx {
            Some(tx) => match tx.try_send(frame) {
                Ok(()) => Ok(None),
                Err(tokio::sync::mpsc::error::TrySendError::Full(f)) => {
                    let (p, slot) =
                        Promise::create_with_channel::<()>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;
                    self.pending.lock().unwrap().push_back((slot, f));
                    self.arm_drain_watcher();
                    Ok(Some(p.into()))
                }
                // Closed: the stream is gone — fire disconnect callbacks once.
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    if let Some(cbs) = &self.close_cbs {
                        for cb in cbs.borrow_mut().drain(..) {
                            if cb.call(ctx, JSValue::UNDEFINED, &[]).is_err() {
                                let e = ctx.take_exception();
                                eprintln!("Error calling onClose callback: {:?}", e);
                            }
                        }
                    }

                    Ok(None)
                }
            },
            // Already shut down.
            None => Ok(None),
        }
    }

    /// Spawn a tokio task that waits for channel capacity and flushes the
    /// parked backlog, resolving each Promise as its frame is sent.
    fn arm_drain_watcher(&self) {
        let tx = self.tx.clone();
        let pending = self.pending.clone();
        tokio::spawn(async move {
            loop {
                let item = pending.lock().unwrap().pop_front();
                let (slot_tx, frame) = match item {
                    Some(x) => x,
                    None => break,
                };
                match &tx {
                    Some(t) => match t.reserve().await {
                        Ok(permit) => {
                            permit.send(frame);
                            // Resolve the parked write Promise.
                            let _ = slot_tx.send(());
                        }
                        // Channel closed — abandon the backlog.
                        Err(_) => break,
                    },
                    // Buffer shut down — abandon the backlog.
                    None => break,
                }
            }
        });
    }
}

impl GcTrace for OutboundBuffer {
    fn trace(&self, tr: &mut GcTraceCtx) {
        if let Some(o) = &self.on_drain {
            o.trace(tr);
        }
    }
}

/// Unified JS write entry point for WS (`WebSocketStream.write`) and SSE
/// (`HttpResponseSse.write`). Returns `Some(promise)` when the caller must
/// await drain, `None` when the frame was enqueued synchronously.
pub(crate) fn js_stream_write(
    buf: &mut OutboundBuffer,
    ctx: &mut ScopedContext,
    frame: String,
) -> Result<Option<JSValue>, i32> {
    let ret = buf.send(ctx, frame)?;
    let pending_empty = buf.pending.lock().unwrap().is_empty();
    if pending_empty && buf.drain_notified {
        // Backlog just cleared by the drain watcher → fire on_drain once.
        buf.drain_notified = false;
        if let Some(cb) = buf.on_drain {
            cb.call(ctx, JSValue::UNDEFINED, &[])?;
        }
    } else if !pending_empty {
        // Entered (or still in) backlog → remember to notify when it clears.
        buf.drain_notified = true;
    }

    Ok(ret)
}
