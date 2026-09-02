// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Log domain methods (flat on `Page`).
//!
//! `onLog(callback)` / `getConsoleMessages(clear?)`. Underlying CDP: `Log.*`
//! (plus `Runtime.consoleAPICalled` for richer console entries).

use std::sync::{LazyLock, Mutex};

use zuqe::{CallContext, JSResult, JSValue, Object, ScopedContext};

use crate::browser::class_page::{get_page_payload, send_page_void_cmd};

/// Per-session ring buffer of console/log entries.
static LOG_BUFFER: LazyLock<Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// `page.onLog(callback)` — enable `Log` and invoke `callback(entry)` for each
/// `Log.entryAdded` (and `Runtime.consoleAPICalled`) event. Each entry is also
/// appended to a per-session buffer readable via `getConsoleMessages()`.
pub(crate) fn js_page_on_log(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    // Store callback in a thread-local keyed by session. The callback is held
    // as a traced `Object` so the GC keeps the JS function alive while it lives
    // in this native-side map; otherwise the node gets freed and the poller
    // below crashes later with a dangling (dtype != Object) pointer.
    if let Some(callback) = cc.arg(0).as_object() {
        CONSOLE_CALLBACK.with(|m| {
            m.borrow_mut().insert(session_id.clone(), callback);
        });
    }

    let _ = send_page_void_cmd(ctx, &cc, "Log.enable", None);

    let (tx, rx) = ctx.make_local_channel::<serde_json::Value>();
    let client_ev = client.clone();
    let session_ev = session_id.clone();
    tokio::spawn(async move {
        let mut event_rx = client_ev.subscribe_events();
        loop {
            match event_rx.recv().await {
                Ok(evt)
                    if (evt.method == "Log.entryAdded"
                        || evt.method == "Runtime.consoleAPICalled")
                        && evt.session_id.as_deref() == Some(&session_ev) =>
                {
                    // Buffer the entry.
                    if let Ok(mut guard) = LOG_BUFFER.lock() {
                        let buf = guard.entry(session_ev.clone()).or_default();
                        buf.push(evt.params.clone());
                    }
                    let msg = serde_json::json!({
                        "sessionId": evt.session_id,
                        "params": evt.params,
                    });
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    crate::helpers::start_channel_poller(
        ctx,
        "log",
        rx,
        move |msg, cx| {
            let sess = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let params = msg
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let cb =
                CONSOLE_CALLBACK.with(|m| sess.as_ref().and_then(|s| m.borrow().get(s).cloned()));
            if let (Some(cb), Some(_sess)) = (cb, sess) {
                let obj = JSValue::from_serde_json(cx, &params).unwrap_or(JSValue::UNDEFINED);
                if cb.call(cx, JSValue::UNDEFINED, &[obj]).is_err() {
                    let exc = cx.take_exception();
                    #[cfg(debug_assertions)]
                    crate::helpers::print_exc(cx, exc);
                }
            }

            true
        },
        || true,
        || {},
    );

    Ok(JSValue::UNDEFINED)
}

thread_local! {
    pub(crate) static CONSOLE_CALLBACK: std::cell::RefCell<std::collections::HashMap<String, Object>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Keep the console-log subscriber callbacks alive across GC cycles.
///
/// The callbacks live in a native-side thread-local map that the GC cannot
/// see, so they must be explicitly traced here (registered in `main.rs`).
/// `page.getConsoleMessages(clear?)` — return the buffered console/log entries
/// for this page. Pass `true` to clear the buffer after reading.
pub(crate) fn js_page_get_console_messages(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let clear = cc.length() > 0 && cc.arg(0).to_bool().unwrap_or(false);
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let session_id = payload.session_id.clone();

    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Ok(mut guard) = LOG_BUFFER.lock()
        && let Some(buf) = guard.get_mut(&session_id)
    {
        messages = std::mem::take(buf);
    }

    if !clear {
        // Put them back if not clearing.
        if let Ok(mut guard) = LOG_BUFFER.lock() {
            guard.insert(session_id, messages.clone());
        }
    }

    JSValue::from_serde_json(ctx, &serde_json::Value::Array(messages))
}
