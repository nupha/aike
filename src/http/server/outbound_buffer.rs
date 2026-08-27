// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

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

use tokio::sync::mpsc;
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
    pub(crate) tx: Option<mpsc::Sender<String>>,
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
}

impl OutboundBuffer {
    /// Create a new buffer together with the `Receiver` the consumer
    /// (bridge task or axum `Sse`) reads frames from.
    pub(crate) fn new(cap: usize) -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel(cap);
        (
            Self {
                tx: Some(tx),
                pending: Arc::new(Mutex::new(VecDeque::new())),
                on_drain: None,
                drain_notified: false,
                close_cbs: None,
            },
            rx,
        )
    }

    /// Wrap a sender that was created earlier (e.g. at WS upgrade time).
    pub(crate) fn from_existing(tx: mpsc::Sender<String>) -> Self {
        Self {
            tx: Some(tx),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            on_drain: None,
            drain_notified: false,
            close_cbs: None,
        }
    }

    /// Attach a shared close-callback list. Fired on first channel-close.
    pub(crate) fn set_close_cbs(&mut self, cbs: class_request::CloseCbs) {
        self.close_cbs = Some(cbs);
    }

    /// Drop the sender so the underlying stream ends (SSE close / WS close).
    pub(crate) fn shutdown(&mut self) {
        self.tx.take();
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
                Err(mpsc::error::TrySendError::Full(f)) => {
                    let (p, slot) =
                        Promise::create_with_channel::<()>(ctx, |_cx, _| Ok(JSValue::UNDEFINED))?;
                    self.pending.lock().unwrap().push_back((slot, f));
                    self.arm_drain_watcher();
                    Ok(Some(p.into()))
                }
                // Closed: the stream is gone — fire disconnect callbacks once.
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if let Some(cbs) = &self.close_cbs {
                        if let Ok(mut guard) = cbs.lock() {
                            for cb in guard.drain(..) {
                                let _ = cb.call(ctx, JSValue::UNDEFINED, &[]);
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
