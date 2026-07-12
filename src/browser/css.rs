// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — CSS domain methods (flat on `Page`).
//!
//! `getComputedStyle` / `getMatchedStyles` — read computed / matched CSS for a
//! node resolved from a selector.

use serde_json::Value;
use zuqe::{CallContext, JSResult, ScopedContext, builtins::promise::{Promise, PromiseResultSlot}};

use crate::browser::{
    class_page::{get_page_payload, js_from_json_value},
    dom,
};

/// `page.getComputedStyle(selector)` — `CSS.getComputedStyleForNode`.
/// Returns a plain object mapping each CSS property name to its computed value.
pub(crate) fn js_page_get_computed_style(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            js_from_json_value(cx, &val)
        })?;

    tokio::spawn(async move {
        let r: Result<Value, String> = async {
            let node_id = dom::resolve_selector(&client, &session_id, &selector).await?;
            let _ = client.send("CSS.enable", None, Some(&session_id)).await;
            let m = client
                .send(
                    "CSS.getComputedStyleForNode",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            // Convert [{ name, value }, …] → { name: value, … }.
            let entries = m
                .get("computedStyle")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut map = serde_json::Map::new();
            for e in &entries {
                if let (Some(name), Some(value)) = (
                    e.get("name").and_then(|v| v.as_str()),
                    e.get("value"),
                ) {
                    map.insert(name.to_string(), value.clone());
                }
            }
            Ok(Value::Object(map))
        }
        .await;
        result_slot.commit(r).ok();
    });

    Ok(promise.into())
}

/// `page.getMatchedStyles(selector)` — `CSS.getMatchedStylesForNode`.
/// Returns the full CDP result (matched rules, inline style, etc.) as JSON.
pub(crate) fn js_page_get_matched_styles(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            js_from_json_value(cx, &val)
        })?;

    tokio::spawn(async move {
        let r: Result<Value, String> = async {
            let node_id = dom::resolve_selector(&client, &session_id, &selector).await?;
            let _ = client.send("CSS.enable", None, Some(&session_id)).await;
            let m = client
                .send(
                    "CSS.getMatchedStylesForNode",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            Ok(m)
        }
        .await;
        result_slot.commit(r).ok();
    });

    Ok(promise.into())
}
