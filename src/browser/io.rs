// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — IO domain (flat on `Page`).
//!
//! `readStream(handle)` — read a CDP IO stream (`IO.read`) to completion and
//! return its full contents as a string. Also used internally by the Tracing
//! module to drain a trace stream.

use base64::Engine as _;
use zuqe::{CallContext, JSResult, JSValue, ScopedContext, builtins::promise::{Promise, PromiseResultSlot}};

use crate::browser::{cdp::CdpClient, class_page::get_page_payload};

/// Read a CDP IO stream (`handle`) to completion and return its UTF-8 text.
/// Loops `IO.read` with an advancing offset until `eof`. Handles both raw and
/// base64-encoded chunks.
pub(crate) async fn read_io_stream(
    client: &CdpClient,
    session_id: &str,
    handle: &str,
) -> Result<String, String> {
    let mut offset: u64 = 0;
    let chunk: u64 = 1 << 16; // 64 KiB per read
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let rx = client.send(
            "IO.read",
            Some(serde_json::json!({ "handle": handle, "offset": offset, "size": chunk })),
            Some(session_id),
        );
        let resp = rx.await.unwrap_or(Err("channel closed".into()))?;

        let data = resp.get("data").and_then(|v| v.as_str()).unwrap_or("");
        let base64 = resp.get("base64Encoded").and_then(|v| v.as_bool()).unwrap_or(false);
        let written = if base64 {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap_or_default();
            buf.extend_from_slice(&bytes);
            bytes.len()
        } else {
            let bytes = data.as_bytes();
            buf.extend_from_slice(bytes);
            bytes.len()
        };

        let eof = resp.get("eof").and_then(|v| v.as_bool()).unwrap_or(false);
        if eof {
            break;
        }
        offset += written as u64;
    }

    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `page.readStream(handle)` — read a stream handle to a string.
pub(crate) fn js_page_read_stream(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let handle = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

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
            cx.create_string_from_utf8(text.into_bytes()).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let r = read_io_stream(&client, &session_id, &handle).await;
        result_slot.commit(r).ok();
    });

    Ok(promise.into())
}
