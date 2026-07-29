// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Storage domain methods (flat on `Page`).
//!
//! `clearData` (clear cache/storage for an origin) and
//! `getStorageKeyForFrame` (resolve a frame's storage key).

use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::class_page::{get_page_payload, send_page_void_cmd};

/// `page.clearData(origin, storageTypes?)` — `Storage.clearDataForOrigin`.
/// `storageTypes` defaults to `"all"` (cookies, localStorage, cache, …).
pub(crate) fn js_page_clear_data(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let origin = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let types = if cc.length() > 1 {
        cc.arg(1)
            .to_string(ctx)
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_else(|_| "all".to_string())
    } else {
        "all".to_string()
    };
    send_page_void_cmd(
        ctx,
        &cc,
        "Storage.clearDataForOrigin",
        Some(serde_json::json!({ "origin": origin, "storageTypes": types })),
    )
}

/// `page.getStorageKeyForFrame(frameId)` — `Storage.getStorageKeyForFrame`.
/// Returns `{ storageKey }`.
pub(crate) fn js_page_get_storage_key_for_frame(
    ctx: &mut ScopedContext,
    cc: CallContext,
) -> JSResult {
    let frame_id = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| cx.throw_internal_error(&e))?;

            // Return the storageKey string directly for ergonomics.
            if let Some(k) = val.get("storageKey").and_then(|v| v.as_str()) {
                cx.create_string_from_utf8(k.as_bytes().to_vec())
                    .map(JSValue::make_string)
            } else {
                Ok(JSValue::NULL)
            }
        })?;

    tokio::spawn(async move {
        let rx = client.send(
            "Storage.getStorageKeyForFrame",
            Some(serde_json::json!({ "frameId": frame_id })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}
