// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! CDP (Chrome DevTools Protocol) client over WebSocket.
//!
//! A background tokio task owns the WebSocket connection.
//! JS threads submit commands via crossbeam channel, each command gets
//! a `tokio::sync::oneshot` response channel, and the background task
//! dispatches responses by `id`.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
};

use crossbeam::channel::{Receiver, Sender, TryRecvError, unbounded};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::{broadcast, oneshot},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWriter = futures_util::stream::SplitSink<WsStream, Message>;
type WsReader = futures_util::stream::SplitStream<WsStream>;

/// A CDP event received from the browser (message with "method" but no "id").
#[derive(Clone, Debug)]
pub(crate) struct CdpEvent {
    pub(crate) method: String,
    pub(crate) params: serde_json::Value,
    pub(crate) session_id: Option<String>,
}

/// A command submitted from JS to the background WebSocket task.
struct CdpCommand {
    msg: String,
    resp_tx: oneshot::Sender<Result<serde_json::Value, String>>,
}

/// Thread-safe handle to the CDP connection.
#[derive(Clone)]
pub(crate) struct CdpClient {
    cmd_tx: Sender<CdpCommand>,
    next_id: Arc<AtomicU64>,
    event_tx: broadcast::Sender<CdpEvent>,
}

impl CdpClient {
    /// Connect to Chrome's DevTools WebSocket and spawn background reader/writer.
    pub(crate) async fn connect(
        ws_url: &str,
    ) -> Result<(Self, tokio::task::JoinHandle<()>), String> {
        let (ws_stream, _) = connect_async(ws_url)
            .await
            .map_err(|e| format!("CDP WebSocket connect failed: {e}"))?;
        let (writer, reader) = ws_stream.split();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));

        let (cmd_tx, cmd_rx) = unbounded::<CdpCommand>();
        let next_id = Arc::new(AtomicU64::new(1));
        let (event_tx, _) = broadcast::channel::<CdpEvent>(256);

        let bg_writer = writer.clone();
        let bg_event_tx = event_tx.clone();
        let handle = tokio::spawn(async move {
            Self::run_background(bg_writer, reader, cmd_rx, bg_event_tx).await;
        });

        Ok((
            Self {
                cmd_tx,
                next_id,
                event_tx,
            },
            handle,
        ))
    }

    /// Background loop: forward commands to WS and dispatch responses / events.
    async fn run_background(
        writer: Arc<tokio::sync::Mutex<WsWriter>>,
        mut reader: WsReader,
        cmd_rx: Receiver<CdpCommand>,
        event_tx: broadcast::Sender<CdpEvent>,
    ) {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_r = pending.clone();
        let event_tx_r = event_tx.clone();
        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = reader.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if !Self::try_dispatch_response(&pending_r, &text) {
                            Self::dispatch_event(&event_tx_r, &text);
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Err(e) => {
                        log::warn!("CDP WS read error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        });

        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    Self::send_cmd(&writer, cmd, &pending).await;
                }
                Err(TryRecvError::Empty) => {
                    if reader_handle.is_finished() {
                        log::warn!("CDP reader task finished, shutting down");
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(TryRecvError::Disconnected) => {
                    log::debug!("CDP command channel closed, shutting down");
                    break;
                }
            }
        }
    }

    /// Send a single CDP command over WebSocket.
    async fn send_cmd(
        writer: &Arc<tokio::sync::Mutex<WsWriter>>,
        cmd: CdpCommand,
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>,
    ) {
        let id: u64 = if let Ok(val) = serde_json::from_str::<serde_json::Value>(&cmd.msg) {
            val.get("id").and_then(|v| v.as_u64()).unwrap_or(0)
        } else {
            0
        };

        if id > 0 {
            pending.lock().unwrap().insert(id, cmd.resp_tx);
        }

        if let Err(e) = writer
            .lock()
            .await
            .send(Message::Text(cmd.msg.clone().into()))
            .await
        {
            log::error!("CDP WS send error: {e}");
        }
    }

    /// Parse a received WS text message and dispatch to the matching pending command.
    /// Returns `true` if it was a command response (had "id"), `false` if it was not.
    fn try_dispatch_response(
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, String>>>>>,
        text: &str,
    ) -> bool {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(text) else {
            return false;
        };

        let id = match val.get("id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return false, // Not a command response, it's an event
        };

        let tx = pending.lock().unwrap().remove(&id);
        if let Some(tx) = tx {
            let result = if let Some(error) = val.get("error") {
                Err(error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("CDP error")
                    .to_string())
            } else {
                Ok(val.get("result").cloned().unwrap_or(serde_json::Value::Null))
            };
            let _ = tx.send(result);
        }
        true
    }

    /// Forward a CDP event (message without "id") to the broadcast channel.
    fn dispatch_event(event_tx: &broadcast::Sender<CdpEvent>, text: &str) {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        let method = match val.get("method").and_then(|v| v.as_str()) {
            Some(m) => m.to_string(),
            None => return,
        };
        let params = val.get("params").cloned().unwrap_or(serde_json::Value::Null);
        let session_id = val
            .get("sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let _ = event_tx.send(CdpEvent {
            method,
            params,
            session_id,
        });
    }

    /// Build a CDP JSON-RPC command message string.
    fn build_msg(method: &str, params: Option<serde_json::Value>, session_id: Option<&str>, id: u64) -> String {
        let mut msg = serde_json::json!({
            "id": id,
            "method": method,
        });
        if let Some(p) = params {
            msg["params"] = p;
        }
        if let Some(sid) = session_id {
            msg["sessionId"] = serde_json::Value::String(sid.to_string());
        }
        serde_json::to_string(&msg).unwrap()
    }

    /// Send a CDP command and return a oneshot receiver for the response.
    pub(crate) fn send(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        session_id: Option<&str>,
    ) -> oneshot::Receiver<Result<serde_json::Value, String>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = Self::build_msg(method, params, session_id, id);
        let (tx, rx) = oneshot::channel();

        let _ = self.cmd_tx.send(CdpCommand { msg, resp_tx: tx });

        rx
    }

    /// Subscribe to CDP events (messages without "id").
    /// Returns a receiver that gets all CDP events from all sessions.
    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<CdpEvent> {
        self.event_tx.subscribe()
    }

    /// Create a new target (tab) and attach a session to it.
    /// Returns (target_id, session_id).
    pub(crate) async fn create_target(&self, url: &str) -> Result<(String, String), String> {
        let rx = self.send(
            "Target.createTarget",
            Some(serde_json::json!({
                "url": url,
            })),
            None,
        );
        let resp = rx.await.map_err(|_| "channel closed".to_string())?;
        let result = resp?;

        let target_id = result
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "no targetId in Target.createTarget response".to_string())?
            .to_string();

        let attach_rx = self.send(
            "Target.attachToTarget",
            Some(serde_json::json!({
                "targetId": target_id,
                "flatten": true,
            })),
            None,
        );
        let attach_resp = attach_rx.await.map_err(|_| "channel closed".to_string())?;
        let attach_result = attach_resp?;

        let session_id = attach_result
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "no sessionId in Target.attachToTarget response".to_string())?
            .to_string();

        Ok((target_id, session_id))
    }

    /// Close a target (tab).
    pub(crate) async fn close_target(&self, target_id: &str) -> Result<(), String> {
        let rx = self.send(
            "Target.closeTarget",
            Some(serde_json::json!({ "targetId": target_id })),
            None,
        );
        let resp = rx.await.map_err(|_| "channel closed".to_string())?;
        resp?;
        Ok(())
    }
}
