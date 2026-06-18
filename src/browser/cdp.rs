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
use serde_json::Value;
use tokio::{net::TcpStream, sync::oneshot};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWriter = futures_util::stream::SplitSink<WsStream, Message>;
type WsReader = futures_util::stream::SplitStream<WsStream>;

/// A command submitted from JS to the background WebSocket task.
struct CdpCommand {
    msg: String,
    resp_tx: oneshot::Sender<Result<Value, String>>,
}

/// Thread-safe handle to the CDP connection.
#[derive(Clone)]
pub(crate) struct CdpClient {
    cmd_tx: Sender<CdpCommand>,
    next_id: Arc<AtomicU64>,
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

        let bg_writer = writer.clone();
        let handle = tokio::spawn(async move {
            Self::run_background(bg_writer, reader, cmd_rx).await;
        });

        Ok((Self { cmd_tx, next_id }, handle))
    }

    /// Background loop: forward commands to WS and dispatch responses.
    async fn run_background(
        writer: Arc<tokio::sync::Mutex<WsWriter>>,
        mut reader: WsReader,
        cmd_rx: Receiver<CdpCommand>,
    ) {
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_r = pending.clone();
        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = reader.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        Self::dispatch_response(&pending_r, &text);
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
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    ) {
        let id: u64 = if let Ok(val) = serde_json::from_str::<Value>(&cmd.msg) {
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
    fn dispatch_response(
        pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
        text: &str,
    ) {
        let Ok(val) = serde_json::from_str::<Value>(text) else {
            return;
        };

        let id = match val.get("id").and_then(|v| v.as_u64()) {
            Some(id) => id,
            None => return,
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
                Ok(val.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = tx.send(result);
        }
    }

    /// Build a CDP JSON-RPC command message string.
    fn build_msg(method: &str, params: Option<Value>, session_id: Option<&str>, id: u64) -> String {
        let mut msg = serde_json::json!({
            "id": id,
            "method": method,
        });
        if let Some(p) = params {
            msg["params"] = p;
        }
        if let Some(sid) = session_id {
            msg["sessionId"] = Value::String(sid.to_string());
        }
        serde_json::to_string(&msg).unwrap()
    }

    /// Send a CDP command and return a oneshot receiver for the response.
    pub(crate) fn send(
        &self,
        method: &str,
        params: Option<Value>,
        session_id: Option<&str>,
    ) -> oneshot::Receiver<Result<Value, String>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let msg = Self::build_msg(method, params, session_id, id);
        let (tx, rx) = oneshot::channel();

        let _ = self.cmd_tx.send(CdpCommand { msg, resp_tx: tx });

        rx
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
