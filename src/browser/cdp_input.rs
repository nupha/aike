// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! CDP Input domain helpers — mouse, keyboard, text input.

use crate::browser::cdp::CdpClient;

/// Dispatch a full mouse click (mousePressed + mouseReleased) at (x,y).
pub(crate) async fn mouse_click(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
    button: &str, // "left" | "middle" | "right"
    click_count: i32,
) -> Result<(), String> {
    let params = serde_json::json!({
        "type": "mousePressed",
        "x": x,
        "y": y,
        "button": button,
        "clickCount": click_count,
    });
    let rx = client.send("Input.dispatchMouseEvent", Some(params), Some(session_id));
    rx.await
        .map_err(|_| "channel closed".to_string())?
        .map(|_| ())?;

    let params = serde_json::json!({
        "type": "mouseReleased",
        "x": x,
        "y": y,
        "button": button,
        "clickCount": click_count,
    });
    let rx = client.send("Input.dispatchMouseEvent", Some(params), Some(session_id));
    rx.await
        .map_err(|_| "channel closed".to_string())?
        .map(|_| ())?;

    Ok(())
}

/// Move mouse to (x,y).
pub(crate) async fn mouse_move(
    client: &CdpClient,
    session_id: &str,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let params = serde_json::json!({
        "type": "mouseMoved",
        "x": x,
        "y": y,
    });
    let rx = client.send("Input.dispatchMouseEvent", Some(params), Some(session_id));
    rx.await
        .map_err(|_| "channel closed".to_string())?
        .map(|_| ())?;
    Ok(())
}

/// Dispatch a key event (keyDown or keyUp).
pub(crate) async fn dispatch_key_event(
    client: &CdpClient,
    session_id: &str,
    event_type: &str, // "keyDown" | "keyUp" | "rawKeyDown" | "char"
    key: &str,
    windows_virtual_key_code: i32,
    modifiers: i32,
) -> Result<(), String> {
    let mut params = serde_json::json!({
        "type": event_type,
        "key": key,
        "windowsVirtualKeyCode": windows_virtual_key_code,
        "modifiers": modifiers,
    });
    if event_type == "char" || event_type == "keyDown" {
        params["text"] = serde_json::json!(key);
    }
    let rx = client.send("Input.dispatchKeyEvent", Some(params), Some(session_id));
    rx.await
        .map_err(|_| "channel closed".to_string())?
        .map(|_| ())?;
    Ok(())
}

/// Insert text via Input.insertText (reliable for CJK / Unicode).
pub(crate) async fn insert_text(
    client: &CdpClient,
    session_id: &str,
    text: &str,
) -> Result<(), String> {
    let params = serde_json::json!({ "text": text });
    let rx = client.send("Input.insertText", Some(params), Some(session_id));
    rx.await
        .map_err(|_| "channel closed".to_string())?
        .map(|_| ())?;
    Ok(())
}
