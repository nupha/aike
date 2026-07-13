// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 5-2026 John Ray <996351336@qq.com>

//! Page class — Tracing domain (flat on `Page`).
//!
//! `startTracing` / `stopTracing` — capture a Chrome trace. The trace is
//! returned as a stream handle in `Tracing.tracingComplete`; `stopTracing`
//! drains it via the IO module's `read_io_stream`.

use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;
use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::{class_page::get_page_payload, io::read_io_stream};

/// `page.startTracing(categories?)` — `Tracing.start`.
/// `categories` is a comma-separated CDP category filter (default: all).
pub(crate) fn js_page_start_tracing(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let categories = if cc.length() > 0 {
        cc.arg(0)
            .to_string(ctx)
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<(), String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let mut params = serde_json::json!({
            "bufferUsageReportingInterval": 0,
            "transferMode": "ReturnAsStream",
            "streamCompression": "none",
            "options": "simplifyNames",
        });
        if !categories.is_empty() {
            params["categories"] = Value::String(categories);
        }
        let rx = client.send("Tracing.start", Some(params), Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

/// `page.stopTracing()` — `Tracing.end`, then wait for `Tracing.tracingComplete`
/// and drain the returned stream to a string. Returns the trace text (may be
/// empty if nothing was captured).
pub(crate) fn js_page_stop_tracing(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<String, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, text| -> JSResult {
            let text = text.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::from)
        })?;

    tokio::spawn(async move {
        // End tracing; this triggers `Tracing.tracingComplete` with a stream handle.
        let end_rx = client.send("Tracing.end", None, Some(&session_id));
        let _ = end_rx.await;

        // Wait for the completion event carrying the stream handle.
        let mut event_rx = client.subscribe_events();
        let session_clone = session_id.clone();
        let mut stream_handle: Option<String> = None;
        let wait = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(wait);
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    match event {
                        Ok(evt)
                            if evt.method == "Tracing.tracingComplete"
                                && evt.session_id.as_deref() == Some(&session_clone) =>
                        {
                            stream_handle = evt
                                .params
                                .get("stream")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            break;
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = &mut wait => break,
            }
        }

        let trace = match stream_handle {
            Some(h) => read_io_stream(&client, &session_id, &h)
                .await
                .unwrap_or_default(),
            None => String::new(),
        };
        result_slot.commit(Ok(trace)).ok();
    });

    Ok(promise.into())
}
