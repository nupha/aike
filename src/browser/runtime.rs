// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Runtime domain methods — flat on `Page`
//! (`page.evaluate` / `page.getOuterHTML` / `page.waitForSelector` /
//! `page.enableRuntime`). The underlying CDP commands are `Runtime.*`.
//!
//! Shared Page plumbing (`get_page_payload`, `js_from_json_value`,
//! `send_page_void_cmd`) lives in `class_page.rs` and is imported here.

use serde_json::Value;
use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::class_page::{get_page_payload, js_from_json_value, send_page_void_cmd};

/// `page.evaluate(jsCode)`
pub(crate) fn js_page_evaluate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let js_code = cc
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
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let val = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

            if let Some(exception) = val.get("exceptionDetails") {
                let text = exception
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("JS exception");
                cx.throw_internal_error(text);
                return Err(-1);
            }

            if let Some(result) = val.get("result") {
                if let Some(value) = result.get("value") {
                    return js_from_json_value(cx, value);
                }
                if result.get("objectId").is_some()
                    && let Some(desc) = result.get("description").and_then(|v| v.as_str())
                {
                    return cx
                        .create_string_from_utf8(desc.as_bytes().to_vec())
                        .map(JSValue::from);
                }
                if let Some(desc) = result.get("description").and_then(|v| v.as_str()) {
                    return cx
                        .create_string_from_utf8(desc.as_bytes().to_vec())
                        .map(JSValue::from);
                }
            }

            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": js_code,
                "awaitPromise": true,
                "returnByValue": true,
            })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// `page.getOuterHTML()`
pub(crate) fn js_page_get_outer_html(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<String, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let html = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(html.into_bytes())
                .map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "document.documentElement.outerHTML",
            })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let html = val
                    .get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                result_slot.commit(Ok(html.to_string())).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `page.waitForSelector(selector, timeoutMs?)`
pub(crate) fn js_page_wait_for_selector(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let timeout_ms: i64 = if cc.length() > 1 {
        let n = cc
            .arg(1)
            .to_number(ctx)
            .map(|n| n.to_i32())
            .unwrap_or(30000);
        n as i64
    } else {
        30000
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
        let js_code = format!(
            "new Promise((resolve, reject) => {{\
             let el = document.querySelector('{}');\
             if (el) {{ resolve(true); return; }}\
             let mo = new MutationObserver(() => {{\
             if (document.querySelector('{}')) {{\
             mo.disconnect(); resolve(true);\
             }}\
             }});\
             mo.observe(document.body, {{ childList: true, subtree: true }});\
             setTimeout(() => {{ mo.disconnect(); reject(new Error('timeout')); }}, {});\
             }})",
            selector.replace('\'', "\\'"),
            selector.replace('\'', "\\'"),
            timeout_ms
        );

        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": js_code,
                "awaitPromise": true,
            })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                if val.get("exceptionDetails").is_some() {
                    result_slot
                        .commit(Err("selector not found or timed out".into()))
                        .ok();
                } else {
                    result_slot.commit(Ok(())).ok();
                }
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `page.enableRuntime()`
pub(crate) fn js_page_enable_runtime(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Runtime.enable", None)
}
