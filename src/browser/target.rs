// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Browser class — Target domain methods (flat on `Browser`).
//!
//! `listPages` / `getTargets` / `newContext` / `disposeContext`.
//! Underlying CDP: `Target.*`.

use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::class_browser::get_browser_payload;

/// `browser.getTargets()` — `Target.getTargets`. Returns the raw target list.
pub(crate) fn js_browser_get_targets(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
        })?;

    tokio::spawn(async move {
        let rx = client.send("Target.getTargets", None, None);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// `browser.listPages()` — page-type targets only
/// (`[{ targetId, url, title, type }, ...]`).
pub(crate) fn js_browser_list_pages(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
        })?;

    tokio::spawn(async move {
        let rx = client.send("Target.getTargets", None, None);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(v) => {
                let pages = v
                    .get("targetInfos")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter(|t| t.get("type").and_then(|x| x.as_str()) == Some("page"))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                result_slot.commit(Ok(serde_json::Value::Array(pages))).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `browser.newContext()` — `Target.createBrowserContext`. Returns the
/// new context id (string). Pass it to `browser.newPage({ contextId })`.
pub(crate) fn js_browser_new_context(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();

    let result_slot = PromiseResultSlot::<Result<String, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, id| -> JSResult {
            let id = id.map_err(|e| cx.throw_internal_error(&e))?;
            cx.create_string_from_utf8(id.into_bytes())
                .map(JSValue::make_string)
        })?;

    tokio::spawn(async move {
        let rx = client.send("Target.createBrowserContext", None, None);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(v) => {
                let id = v
                    .get("browserContextId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                result_slot.commit(Ok(id)).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `browser.disposeContext(contextId)` — `Target.disposeBrowserContext`.
pub(crate) fn js_browser_dispose_context(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let context_id = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();

    let result_slot = PromiseResultSlot::<Result<(), String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            payload.map_err(|e| cx.throw_internal_error(&e))?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let rx = client.send(
            "Target.disposeBrowserContext",
            Some(serde_json::json!({ "browserContextId": context_id })),
            None,
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => {
                result_slot.commit(Ok(())).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        };
    });

    Ok(promise.into())
}
