// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — DOM domain methods (flat on `Page`).
//!
//! `querySelector` / `querySelectorAll` / `getAttribute` / `focus` /
//! `getBoxModel` / `remove` / `click` / `type`. Underlying CDP: `DOM.*`.
//!
//! Shared Page plumbing (`get_page_payload`, `send_page_void_cmd`, prop
//! getters) lives in `class_page.rs`. JSON→JS conversion uses zuqe's
//! `JSValue::from_serde_json`.

use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::{cdp::CdpClient, cdp_input, class_page::get_page_payload};

/// Enable the DOM domain and resolve a CSS selector to a `nodeId`.
/// The context node is the document root obtained via `DOM.getDocument`.
/// Exposed so sibling modules (CSS/Overlay) can reuse selector → nodeId.
pub(crate) async fn resolve_selector(
    client: &CdpClient,
    session_id: &str,
    selector: &str,
) -> Result<i64, String> {
    // Enable DOM (idempotent) so nodeIds are valid.
    let _ = client.send("DOM.enable", None, Some(session_id)).await;

    let doc_rx = client.send(
        "DOM.getDocument",
        Some(serde_json::json!({ "depth": -1 })),
        Some(session_id),
    );
    let doc = doc_rx
        .await
        .unwrap_or(Err("channel closed".into()))?
        .get("root")
        .and_then(|r| r.get("nodeId"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "DOM.getDocument: no root nodeId".to_string())?;

    let q_rx = client.send(
        "DOM.querySelector",
        Some(serde_json::json!({ "nodeId": doc, "selector": selector })),
        Some(session_id),
    );
    let node_id = q_rx
        .await
        .unwrap_or(Err("channel closed".into()))?
        .get("nodeId")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("selector not found: {selector}"))?;
    Ok(node_id)
}

/// `page.querySelector(selector)` → nodeId (number).
pub(crate) fn js_page_query_selector(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<i64, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, node_id| -> JSResult {
            let node_id = node_id.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::from(node_id as i32))
        })?;

    tokio::spawn(async move {
        let r = resolve_selector(&client, &session_id, &selector).await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.querySelectorAll(selector)` → array of nodeIds.
pub(crate) fn js_page_query_selector_all(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<Vec<i64>, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, ids| -> JSResult {
            let ids = ids.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            let vals: Vec<JSValue> = ids.iter().map(|i| JSValue::from(*i as i32)).collect();
            zuqe::Array::from_values(cx, vals).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let _ = client.send("DOM.enable", None, Some(&session_id)).await;
        let r: Result<Vec<i64>, String> = async {
            let doc_rx = client.send(
                "DOM.getDocument",
                Some(serde_json::json!({ "depth": -1 })),
                Some(&session_id),
            );
            let d = doc_rx.await.unwrap_or(Err("channel closed".into()))?;
            let root = d
                .get("root")
                .and_then(|r| r.get("nodeId"))
                .and_then(|v| v.as_i64())
                .ok_or_else(|| "DOM.getDocument: no root nodeId".to_string())?;
            let q_rx = client.send(
                "DOM.querySelectorAll",
                Some(serde_json::json!({ "nodeId": root, "selector": selector })),
                Some(&session_id),
            );
            let q = q_rx.await.unwrap_or(Err("channel closed".into()))?;
            let ids = q
                .get("nodeIds")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default();
            Ok(ids)
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.getAttribute(selector, name)` → attribute value (string|null).
pub(crate) fn js_page_get_attribute(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let name = cc
        .arg(1)
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
            let val = val.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            JSValue::from_serde_json(cx, &val)
        })?;

    tokio::spawn(async move {
        let r: Result<serde_json::Value, String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            let a = client
                .send(
                    "DOM.getAttributes",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            let attrs = a
                .get("attributes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // attributes is a flat [name, value, name, value, ...] list.
            let mut i = 0;
            while i + 1 < attrs.len() {
                if attrs[i].as_str() == Some(&name) {
                    return Ok(attrs[i + 1].clone());
                }
                i += 2;
            }
            Ok(serde_json::Value::Null)
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.focus(selector)` — native DOM focus.
pub(crate) fn js_page_focus(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

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
        let r: Result<(), String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            client
                .send(
                    "DOM.focus",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            Ok(())
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.getBoxModel(selector)` → `{ content, border, padding, margin, width, height }`.
pub(crate) fn js_page_get_box_model(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
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
            let val = val.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            JSValue::from_serde_json(cx, &val)
        })?;

    tokio::spawn(async move {
        let r: Result<serde_json::Value, String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            let m = client
                .send(
                    "DOM.getBoxModel",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            Ok(m.get("model").cloned().unwrap_or(serde_json::Value::Null))
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.remove(selector)` — remove the matched node from the DOM.
pub(crate) fn js_page_remove(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

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
        let r: Result<(), String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            client
                .send(
                    "DOM.removeNode",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            Ok(())
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.click(selector)` — native click: resolve, get box center, dispatch mouse.
pub(crate) fn js_page_click(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

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
        let r: Result<(), String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            let m = client
                .send(
                    "DOM.getBoxModel",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            let quad = m
                .get("model")
                .and_then(|mo| mo.get("content"))
                .and_then(|v| v.as_array())
                .ok_or_else(|| "no box model content".to_string())?;
            let xs: Vec<f64> = quad.iter().step_by(2).filter_map(|v| v.as_f64()).collect();
            let ys: Vec<f64> = quad
                .iter()
                .skip(1)
                .step_by(2)
                .filter_map(|v| v.as_f64())
                .collect();
            if xs.len() < 2 || ys.len() < 2 {
                return Err("malformed box model".to_string());
            }
            let cx = (xs[0] + xs[2.min(xs.len() - 1)]) / 2.0;
            let cy = (ys[0] + ys[2.min(ys.len() - 1)]) / 2.0;
            cdp_input::mouse_click(&client, &session_id, cx, cy, "left", 1).await
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.type(selector, text)` — focus the element, then insert text.
pub(crate) fn js_page_type(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let text = cc
        .arg(1)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

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
        let r: Result<(), String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            client
                .send(
                    "DOM.focus",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            client
                .send(
                    "Input.insertText",
                    Some(serde_json::json!({ "text": text })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            Ok(())
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// Build the aria-node JSON array from a raw `Accessibility.getFullAXTree`
/// result. AX `nodeId`/`parentId` are strings; convert them to numbers so the
/// JS layer can use `nodeId` directly as a ref id (`eN`). Each node carries a
/// stable `backendNodeId` (CDP backend node id) used for ref-based click/type.
fn build_aria_nodes(val: &serde_json::Value) -> String {
    let ax_nodes = val.get("nodes").and_then(|n| n.as_array());
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut idx_of: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    if let Some(ax_nodes) = ax_nodes {
        for (i, n) in ax_nodes.iter().enumerate() {
            let node_id = n
                .get("nodeId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            idx_of.insert(node_id, i);
            let role = n
                .get("role")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = n
                .get("name")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let desc = n
                .get("description")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let text = n
                .get("value")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            out.push(serde_json::json!({
                "nodeId": node_id,
                "backendNodeId": n.get("backendDOMNodeId").and_then(|v| v.as_u64()).unwrap_or(0),
                "parentIndex": -1,
                "contentType": "element",
                "textValue": text,
                "attributes": { "role": role, "name": name, "description": desc },
            }));
        }
    }
    // Resolve parentIndex from parentId (parent node's output index).
    if let Some(ax_nodes) = val.get("nodes").and_then(|n| n.as_array()) {
        for (i, n) in ax_nodes.iter().enumerate() {
            let parent_idx = n
                .get("parentId")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .and_then(|p| idx_of.get(&p).copied())
                .map(|x| x as i64)
                .unwrap_or(-1i64);
            out[i]["parentIndex"] = serde_json::json!(parent_idx);
        }
    }
    serde_json::to_string(&out).unwrap_or_else(|_| "[]".to_string())
}

/// `page.ariaSnapshot()` — builds an aria/accessibility tree via
/// `Accessibility.getFullAXTree`. It yields `role`, `name` and a stable
/// `backendDOMNodeId` per node. Returns a JSON string: an array of aria
/// nodes, each carrying `nodeId` (its ref, e.g. `e12`) and a stable
/// `backendNodeId` (survives DOM mutations).
pub(crate) fn js_page_aria_snapshot(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<String, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let data = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(data.into_bytes())
                .map(JSValue::make_string)
        })?;

    tokio::spawn(async move {
        // Accessibility.getFullAXTree yields role/name and a stable
        // backendDOMNodeId (the ref target) per node.
        let _ = client
            .send("Accessibility.enable", None, Some(&session_id))
            .await;
        let result = client
            .send("Accessibility.getFullAXTree", None, Some(&session_id))
            .await
            .unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let data = build_aria_nodes(&val);
                result_slot.commit(Ok(data)).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        };
    });

    Ok(promise.into())
}

/// `page.clickBackendNode(backendNodeId)` — click the element identified by its
/// CDP backend node id (stable ref from `ariaSnapshot`), without re-querying DOM.
pub(crate) fn js_page_click_backend_node(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let backend_node_id = cc
        .arg(0)
        .to_number(ctx)
        .map(|n| n.to_f64() as u64)
        .unwrap_or(0);

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
        let r: Result<(), String> = async {
            // Enable DOM (idempotent) so backendNodeId resolves to a box model.
            let _ = client.send("DOM.enable", None, Some(&session_id)).await;
            let m = client
                .send(
                    "DOM.getBoxModel",
                    Some(serde_json::json!({ "backendNodeId": backend_node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            let quad = m
                .get("model")
                .and_then(|mo| mo.get("content"))
                .and_then(|v| v.as_array())
                .ok_or_else(|| "no box model content".to_string())?;
            let xs: Vec<f64> = quad.iter().step_by(2).filter_map(|v| v.as_f64()).collect();
            let ys: Vec<f64> = quad
                .iter()
                .skip(1)
                .step_by(2)
                .filter_map(|v| v.as_f64())
                .collect();
            if xs.len() < 2 || ys.len() < 2 {
                return Err("malformed box model".to_string());
            }
            let cx = (xs[0] + xs[2.min(xs.len() - 1)]) / 2.0;
            let cy = (ys[0] + ys[2.min(ys.len() - 1)]) / 2.0;
            cdp_input::mouse_click(&client, &session_id, cx, cy, "left", 1).await
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.typeBackendNode(backendNodeId, text)` — focus the element by backend node id,
/// then insert text (CJK/Unicode safe). Mirrors `page.type` but via a stable ref.
pub(crate) fn js_page_type_backend_node(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let backend_node_id = cc
        .arg(0)
        .to_number(ctx)
        .map(|n| n.to_f64() as u64)
        .unwrap_or(0);
    let text = cc
        .arg(1)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

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
        let r: Result<(), String> = async {
            // Enable DOM (idempotent) so backendNodeId resolves for DOM.focus.
            let _ = client.send("DOM.enable", None, Some(&session_id)).await;
            client
                .send(
                    "DOM.focus",
                    Some(serde_json::json!({ "backendNodeId": backend_node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            cdp_input::insert_text(&client, &session_id, &text).await
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}

/// `page.getNodeValue(selector)` → outerHTML (string) of the single matched node.
pub(crate) fn js_page_get_node_value(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
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
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, html| -> JSResult {
            let html = html.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(html.into_bytes())
                .map(JSValue::make_string)
        })?;

    tokio::spawn(async move {
        let r: Result<String, String> = async {
            let node_id = resolve_selector(&client, &session_id, &selector).await?;
            let m = client
                .send(
                    "DOM.getOuterHTML",
                    Some(serde_json::json!({ "nodeId": node_id })),
                    Some(&session_id),
                )
                .await
                .unwrap_or(Err("channel closed".into()))?;
            m.get("outerHTML")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| "no outerHTML".to_string())
        }
        .await;
        result_slot.commit(r).ok();
    });
    Ok(promise.into())
}
