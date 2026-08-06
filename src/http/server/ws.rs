// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `WebSocketStream` — the JS-side handle for a single WebSocket connection.
//!
//! This is the *only* WS type: it is a real bidirectional long connection with
//! its own lifecycle. Pull (read) is driven by `[Symbol.asyncIterator]` so JS
//! writes `for await (const msg of stream) { ... }`. Push (write) is unified as
//! `write()`, backed by the shared `OutboundBuffer` (see `outbound_buffer.rs`).
//!
//! SSE is deliberately NOT here — SSE is a *response type* (`HttpResponseSse` in
//! `response.rs`), not a connection type.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::extract::ws::{Message, WebSocket};
use crossbeam::channel::{Receiver, Sender, TryRecvError, bounded};
use linkme::distributed_slice;
use tokio::sync::mpsc;
use zuqe::{
    CallContext, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::iterator::{AsyncIterableIterator, AsyncIteratorFetcher, IterFetchStatus},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use super::{
    class_http_server::{IncomingRequest, get_server_payload_mut},
    outbound_buffer::{OutboundBuffer, js_stream_write},
};

use crate::USER_CLASS_DEFS;

/// Capacity of the inbound (client→server) channel feeding the async iterator.
const INBOUND_CAP: usize = 64;

/// Control command sent from the JS `WebSocketStream` to the bridge task.
#[derive(Clone, Copy)]
pub(crate) enum WsControl {
    Close,
    Ping,
}

/// Event delivered from the bridge task to the JS poller.
#[derive(Clone)]
pub(crate) enum WsStreamEvent {
    Message(String),
    Close,
}

/// Per-connection payload carried by the `WebSocketStream` JS object.
pub(crate) struct WsStreamPayload {
    pub(crate) id: u64,
    /// Write side (server→client). Shared component with SSE.
    pub(crate) sink: OutboundBuffer,
    pub(crate) closed: Arc<AtomicBool>,
    /// Topics this stream subscribed to (Bun pub/sub).
    pub(crate) topics: HashSet<String>,
    /// Command channel to the bridge task (Close / Ping).
    pub(crate) control_tx: mpsc::Sender<WsControl>,
    /// Forwarding sender: poller pushes inbound messages here to feed the
    /// async iterator (pull mode). `None` once the connection closes.
    pub(crate) read_tx: Option<Sender<String>>,
    /// Receiver half taken by `[Symbol.asyncIterator]` to build the iterator.
    pub(crate) read_rx: Option<Receiver<String>>,
    // --- push (escape hatch) / lifecycle callbacks ---
    pub(crate) on_open: Option<Object>,
    pub(crate) on_message: Option<Object>,
    pub(crate) on_close: Option<Object>,
    pub(crate) on_error: Option<Object>,
    pub(crate) ready_state: Cell<u8>,
    pub(crate) remote_address: String,
}

impl WsStreamPayload {
    /// GC trace callback wired via `def_native_object!`'s `on_payload_gc`.
    pub(crate) fn gc_trace(&self, tr: &mut GcTraceCtx) {
        self.sink.trace(tr);
        if let Some(o) = &self.on_open {
            o.trace(tr);
        }
        if let Some(o) = &self.on_message {
            o.trace(tr);
        }
        if let Some(o) = &self.on_close {
            o.trace(tr);
        }
        if let Some(o) = &self.on_error {
            o.trace(tr);
        }
    }
}

pub(crate) fn get_ws_payload_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut WsStreamPayload, i32> {
    JSWebSocketStream::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not a WebSocketStream instance"))
}

// ---------------------------------------------------------------------------
// Bridge task: axum WebSocket <-> JS channels
// ---------------------------------------------------------------------------

/// Bridge a single axum WebSocket to the JS-side channels.
///
/// - `out_rx`: frames JS wrote via `stream.write()` → forwarded to the socket.
/// - `control_rx`: `WsControl` commands (Close / Ping) from the JS side.
/// - `event_tx`: client messages / close are reported back to the JS poller as
///   `IncomingRequest::WebSocketData`.
pub(crate) async fn bridge_ws(
    mut socket: WebSocket,
    mut out_rx: mpsc::Receiver<String>,
    mut control_rx: mpsc::Receiver<WsControl>,
    event_tx: Sender<IncomingRequest>,
    id: u64,
) {
    loop {
        tokio::select! {
            maybe_frame = out_rx.recv() => {
                match maybe_frame {
                    Some(frame) => {
                        if socket.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            maybe_msg = socket.recv() => {
                match maybe_msg {
                    Some(Ok(Message::Text(t))) => {
                        let _ = event_tx.send(IncomingRequest::WebSocketData {
                            id,
                            event: WsStreamEvent::Message(t.to_string()),
                        });
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {} // ignore binary / ping / pong
                    Some(Err(_)) => break,
                    None => break, // stream ended
                }
            }
            maybe_ctrl = control_rx.recv() => {
                match maybe_ctrl {
                    Some(WsControl::Close) => {
                        let _ = socket.send(Message::Close(None)).await;
                        break;
                    }
                    Some(WsControl::Ping) => {
                        let _ = socket.send(Message::Ping(Default::default())).await;
                    }
                    None => {}
                }
            }
        }
    }

    // Notify the JS poller that the connection is gone.
    let _ = event_tx.send(IncomingRequest::WebSocketData {
        id,
        event: WsStreamEvent::Close,
    });
}

// ---------------------------------------------------------------------------
// Async iterator (pull) support
// ---------------------------------------------------------------------------

/// Fetcher feeding the `for await` loop from the inbound channel.
/// `close()` on the iterator (break / return) sends `WsControl::Close` to tear
/// down the underlying socket.
struct WsReadFetcher {
    rx: RefCell<Option<Receiver<String>>>,
    control_tx: mpsc::Sender<WsControl>,
}

impl AsyncIteratorFetcher for WsReadFetcher {
    fn try_fetch(&self, cx: &mut ScopedContext) -> Result<IterFetchStatus, i32> {
        let mut g = self.rx.borrow_mut();
        match g.as_ref() {
            Some(rx) => match rx.try_recv() {
                Ok(s) => Ok(IterFetchStatus::Value(JSValue::make_string(
                    cx.create_string_from_str(&s)?,
                ))),
                Err(TryRecvError::Empty) => Ok(IterFetchStatus::Wait),
                Err(TryRecvError::Disconnected) => {
                    g.take();
                    Ok(IterFetchStatus::Done)
                }
            },
            None => Ok(IterFetchStatus::Done),
        }
    }

    fn close(&self) {
        let _ = self.control_tx.try_send(WsControl::Close);
        self.rx.borrow_mut().take();
    }
}

// ---------------------------------------------------------------------------
// Poller-side dispatch (runs on the JS thread)
// ---------------------------------------------------------------------------

/// Handle a fresh WS connection: build the `WebSocketStream` JS object, register
/// it, and invoke the user's `ws(path, handler)`.
pub(crate) fn dispatch_ws_open(
    cx: &mut ScopedContext,
    handler_index: usize,
    id: u64,
    out_tx: mpsc::Sender<String>,
    _query: String,
    _params: HashMap<String, String>,
    control_tx: mpsc::Sender<WsControl>,
    ws_table: &[Object],
    server_val: &mut JSValue,
) {
    let handler = match ws_table.get(handler_index) {
        Some(h) => h,
        None => return,
    };

    let sink = OutboundBuffer::from_existing(out_tx);
    let (read_tx, read_rx) = bounded::<String>(INBOUND_CAP);
    let payload = WsStreamPayload {
        id,
        sink,
        closed: Arc::new(AtomicBool::new(false)),
        topics: HashSet::new(),
        control_tx,
        read_tx: Some(read_tx),
        read_rx: Some(read_rx),
        on_open: None,
        on_message: None,
        on_close: None,
        on_error: None,
        ready_state: Cell::new(1),
        remote_address: String::new(),
    };

    let obj = match JSWebSocketStream::new_instance(cx, JSValue::UNDEFINED, payload) {
        Ok(o) => o,
        Err(_) => return,
    };
    let obj_val: JSValue = obj.clone().into();

    // Register so publish() / close cleanup can find it.
    {
        let server = match get_server_payload_mut(cx, server_val) {
            Ok(s) => s,
            Err(_) => return,
        };
        server.stream_registry.borrow_mut().insert(id, obj.into());
    }

    // Invoke the user handler (synchronous setup runs before we return).
    if let Err(_) = handler.call(cx, JSValue::UNDEFINED, &[obj_val]) {
        let exc = cx.take_exception();
        let msg = exc.to_text(cx);
        if !msg.message.is_empty() {
            eprintln!("[ws] handler error: {}", msg.message);
        }
    }

    // Fire on_open now (after the handler's synchronous setup completed).
    {
        let mut ov = obj_val;
        if let Ok(p) = get_ws_payload_mut(cx, &mut ov) {
            if let Some(cb) = p.on_open.clone()
                && cb.call(cx, ov, &[]).is_err()
            {
                let _e = cx.take_exception();
                #[cfg(debug_assertions)]
                eprintln!("[swallow-exc] ws.rs:{}: {:?}", line!(), _e);
            }
        }
    }
}

/// Handle an inbound WS event (a client message, or the connection closing).
pub(crate) fn dispatch_ws_data(
    cx: &mut ScopedContext,
    id: u64,
    event: WsStreamEvent,
    server_val: &mut JSValue,
) {
    // Pull the Object out of the registry first, releasing the payload borrow
    // before we touch `cx` again (the two would otherwise alias `cx`).
    let obj = {
        let server = match get_server_payload_mut(cx, server_val) {
            Ok(s) => s,
            Err(_) => return,
        };
        match server.stream_registry.borrow().get(&id).cloned() {
            Some(o) => o,
            None => return,
        }
    };
    let mut ov: JSValue = obj.into();

    match event {
        WsStreamEvent::Message(msg) => {
            let p = match get_ws_payload_mut(cx, &mut ov) {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Some(cb) = p.on_message.clone() {
                // push mode: deliver to the callback
                let s = match cx.create_string_from_str(&msg) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                if cb.call(cx, ov, &[JSValue::make_string(s)]).is_err() {
                    let _e = cx.take_exception();
                    #[cfg(debug_assertions)]
                    eprintln!("[swallow-exc] ws.rs:{}: {:?}", line!(), _e);
                }
            } else if let Some(tx) = &p.read_tx {
                // pull mode: forward to the async iterator's channel
                let _ = tx.try_send(msg);
            }
        }
        WsStreamEvent::Close => {
            {
                let p = match get_ws_payload_mut(cx, &mut ov) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                p.closed.store(true, Ordering::Release);
                p.ready_state.set(3);
                p.read_tx = None; // → iterator sees Disconnected → for await ends
                if let Some(cb) = p.on_close.clone()
                    && cb.call(cx, ov, &[]).is_err()
                {
                    let _e = cx.take_exception();
                    #[cfg(debug_assertions)]
                    eprintln!("[swallow-exc] ws.rs:{}: {:?}", line!(), _e);
                }
            }
            // Now re-borrow the payload to drop the connection from the registry.
            if let Ok(server) = get_server_payload_mut(cx, server_val) {
                server.stream_registry.borrow_mut().remove(&id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// JS methods
// ---------------------------------------------------------------------------

fn js_ws_asyncIterator(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    if p.closed.load(Ordering::Acquire) || p.read_rx.is_none() {
        // Already closed or already consumed — return an empty iterator.
        let iter = AsyncIterableIterator::create_with_receiver::<String>(
            ctx,
            bounded::<String>(1).1,
            |_cx, _: String| Ok(None),
        )?;
        return Ok(iter.into());
    }
    let rx = p.read_rx.take().unwrap();
    let control_tx = p.control_tx.clone();
    let iter = AsyncIterableIterator::create(
        ctx,
        WsReadFetcher {
            rx: RefCell::new(Some(rx)),
            control_tx,
        },
    )?;
    Ok(iter.into())
}

fn js_ws_write(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let frame = cc.arg(0).to_string(ctx)?;
    let frame = frame.content().encode_utf8(false).to_string();
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    if p.closed.load(Ordering::Acquire) {
        return Ok(JSValue::UNDEFINED);
    }
    let ret = js_stream_write(&mut p.sink, ctx, frame)?;
    Ok(ret.unwrap_or(JSValue::UNDEFINED))
}

fn js_ws_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    if !p.closed.swap(true, Ordering::AcqRel) {
        p.ready_state.set(3);
        p.read_tx = None;
        let _ = p.control_tx.try_send(WsControl::Close);
    }
    Ok(JSValue::UNDEFINED)
}

fn js_ws_ping(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    let _ = p.control_tx.try_send(WsControl::Ping);
    Ok(JSValue::UNDEFINED)
}

fn js_ws_on_message(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let cb = cc.arg(0).ensure_callable(ctx)?;
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.on_message = Some(cb);
    Ok(this)
}

fn js_ws_on_open(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let cb = cc.arg(0).ensure_callable(ctx)?;
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.on_open = Some(cb);
    Ok(this)
}

fn js_ws_on_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let cb = cc.arg(0).ensure_callable(ctx)?;
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.on_close = Some(cb);
    Ok(this)
}

fn js_ws_on_error(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let cb = cc.arg(0).ensure_callable(ctx)?;
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.on_error = Some(cb);
    Ok(this)
}

fn js_ws_on_drain(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let cb = cc.arg(0).ensure_callable(ctx)?;
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.sink.on_drain = Some(cb);
    Ok(this)
}

fn js_ws_subscribe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let topic = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.topics.insert(topic);
    Ok(this)
}

fn js_ws_unsubscribe(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let topic = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    p.topics.remove(&topic);
    Ok(this)
}

fn js_ws_ready_state(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    Ok(JSValue::from(p.ready_state.get() as u32))
}

fn js_ws_remote_address(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    let s = ctx.create_string_from_str(&p.remote_address)?;
    Ok(JSValue::make_string(s))
}

fn js_ws_id(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let p = get_ws_payload_mut(ctx, &mut this)?;
    Ok(JSValue::from(p.id as f64))
}

const WS_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("[Symbol.asyncIterator]", 0, js_ws_asyncIterator, 0),
    FieldDef::FUNC("write", 1, js_ws_write, 0),
    FieldDef::FUNC("ping", 0, js_ws_ping, 0),
    FieldDef::FUNC("close", 0, js_ws_close, 0),
    FieldDef::FUNC("onMessage", 1, js_ws_on_message, 0),
    FieldDef::FUNC("onOpen", 1, js_ws_on_open, 0),
    FieldDef::FUNC("onClose", 1, js_ws_on_close, 0),
    FieldDef::FUNC("onError", 1, js_ws_on_error, 0),
    FieldDef::FUNC("onDrain", 1, js_ws_on_drain, 0),
    FieldDef::FUNC("subscribe", 1, js_ws_subscribe, 0),
    FieldDef::FUNC("unsubscribe", 1, js_ws_unsubscribe, 0),
    FieldDef::GETSET("readyState", Some(js_ws_ready_state), None, 0),
    FieldDef::GETSET("remoteAddress", Some(js_ws_remote_address), None, 0),
    FieldDef::GETSET("id", Some(js_ws_id), None, 0),
];

def_native_object!(
    "WebSocketStream",
    JSWebSocketStream,
    WsStreamPayload,
    WS_PROTO,
    None,
    on_payload_gc: WsStreamPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_ws_stream(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSWebSocketStream::register(rt)
}
