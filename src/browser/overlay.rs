// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Overlay domain methods (flat on `Page`).
//!
//! `highlight` / `hideHighlight` — debug highlight of a DOM node.

use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::{
    class_page::{get_page_payload, send_page_void_cmd},
    dom,
};

/// Default highlight config: a translucent blue content box plus borders.
fn default_highlight_config() -> serde_json::Value {
    serde_json::json!({
        "showInfo": true,
        "contentColor": { "r": 111, "g": 168, "b": 220, "a": 0.5 },
        "paddingColor": { "r": 147, "g": 196, "b": 125, "a": 0.5 },
        "borderColor": { "r": 255, "g": 229, "b": 153, "a": 0.5 },
        "marginColor": { "r": 246, "g": 178, "b": 107, "a": 0.5 },
    })
}

/// `page.highlight(selector, opts?)` — `Overlay.highlightNode`.
/// `opts` may override `color` (CSS color string) merged into `contentColor`.
pub(crate) fn js_page_highlight(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let config = default_highlight_config();

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
        // Overlay.enable is required before highlightNode in some Chrome builds.
        let en = client.send("Overlay.enable", None, Some(&session_id)).await;
        let r: Result<(), String> = async {
            let _ = en.map_err(|e| format!("Overlay.enable failed: {e}"))?;
            let node_id = dom::resolve_selector(&client, &session_id, &selector).await?;

            let _ = client
                .send(
                    "Overlay.highlightNode",
                    Some(serde_json::json!({ "nodeId": node_id, "highlightConfig": config })),
                    Some(&session_id),
                )
                .await
                .map_err(|e| format!("Overlay.highlightNode failed: {e}"))?;

            Ok(())
        }
        .await;
        result_slot.commit(r).ok();
    });

    Ok(promise.into())
}

/// `page.hideHighlight()` — `Overlay.hideHighlight`.
pub(crate) fn js_page_hide_highlight(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Overlay.hideHighlight", None)
}
