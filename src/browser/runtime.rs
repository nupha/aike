// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Runtime domain methods — flat on `Page`
//! (`page.evaluate` / `page.getOuterHTML` / `page.waitForSelector` /
//! `page.enableRuntime`). The underlying CDP commands are `Runtime.*`.
//!
//! Shared Page plumbing (`get_page_payload`, `js_from_json_value`,
//! `send_page_void_cmd`) lives in `class_page.rs` and is imported here.

use crossbeam::channel;
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

// ---------------------------------------------------------------------------
// Runtime domain — P0/P1
// ---------------------------------------------------------------------------

/// Shallow JSValue -> serde_json::Value (string/number/bool/null).
fn js_value_to_json(ctx: &mut ScopedContext, v: &JSValue) -> Value {
    if let Some(s) = v.as_string() {
        Value::String(s.content().encode_utf8(false).to_string())
    } else if let Some(n) = v.to_number(ctx).ok() {
        Value::from(n.to_f64())
    } else if v.is_bool() {
        Value::Bool(v.to_bool().unwrap_or(false))
    } else if v.is_null() || v.is_undefined() {
        Value::Null
    } else {
        Value::Null
    }
}

/// `page.callFunctionOn(objectId, fn, args?)` — `Runtime.callFunctionOn`.
pub(crate) fn js_page_call_function_on(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let object_id = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let fn_src = cc
        .arg(1)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let mut call_args = Vec::new();
    if cc.length() > 2
        && let Some(arr) = cc.arg(2).as_object_Array()
    {
        for item in arr.payload().values() {
            call_args.push(serde_json::json!({ "value": js_value_to_json(ctx, &item) }));
        }
    }

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
        let rx = client.send(
            "Runtime.callFunctionOn",
            Some(serde_json::json!({
                "objectId": object_id,
                "functionDeclaration": fn_src,
                "arguments": call_args,
                "returnByValue": true,
            })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                if val.get("exceptionDetails").is_some() {
                    result_slot.commit(Err("callFunctionOn: JS exception".into())).ok();
                } else {
                    let r = val
                        .get("result")
                        .and_then(|x| x.get("value"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    result_slot.commit(Ok(r)).ok();
                }
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `page.getProperties(objectId)` — `Runtime.getProperties`.
/// Returns a JS object mapping property name -> value.
pub(crate) fn js_page_get_properties(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let object_id = cc
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
        let rx = client.send(
            "Runtime.getProperties",
            Some(serde_json::json!({ "objectId": object_id })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let props = val
                    .get("result")
                    .and_then(|a| a.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut map = serde_json::Map::new();
                for p in &props {
                    if let Some(name) = p.get("name").and_then(|x| x.as_str()) {
                        let value = p
                            .get("value")
                            .and_then(|x| x.get("value"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        map.insert(name.to_string(), value);
                    }
                }
                result_slot.commit(Ok(Value::Object(map))).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `page.evaluateHandle(expr)` — `Runtime.evaluate` returning an objectId handle.
/// Returns `{ objectId }`.
pub(crate) fn js_page_evaluate_handle(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let expr = cc
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
        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": expr,
                "returnByValue": false,
            })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let obj_id = val
                    .get("result")
                    .and_then(|x| x.get("objectId"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                result_slot
                    .commit(Ok(serde_json::json!({ "objectId": obj_id })))
                    .ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Console event callback
// ---------------------------------------------------------------------------

thread_local! {
    static CONSOLE_CALLBACK: std::cell::RefCell<std::collections::HashMap<String, JSValue>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// `page.onConsole(callback)` — invoke `callback(msg)` for each
/// `Runtime.consoleAPICalled` event. `msg` is the raw event params object.
pub(crate) fn js_page_on_console(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let callback = cc.arg(0).clone();
    CONSOLE_CALLBACK.with(|m| {
        m.borrow_mut().insert(session_id.clone(), callback);
    });

    let _ = send_page_void_cmd(ctx, &cc, "Runtime.enable", None);

    let (tx, rx) = channel::unbounded::<serde_json::Value>();
    let client_ev = client.clone();
    let session_ev = session_id.clone();
    tokio::spawn(async move {
        let mut event_rx = client_ev.subscribe_events();
        loop {
            match event_rx.recv().await {
                Ok(evt)
                    if evt.method == "Runtime.consoleAPICalled"
                        && evt.session_id.as_deref() == Some(&session_ev) =>
                {
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
        "console",
        rx,
        move |msg, cx| {
            let sess = msg
                .get("sessionId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let cb = CONSOLE_CALLBACK
                .with(|m| sess.as_ref().and_then(|s| m.borrow().get(s).cloned()));
            if let (Some(cb), Some(_sess)) = (cb, sess) {
                let obj = js_from_json_value(cx, &params).unwrap_or(JSValue::UNDEFINED);
                let _ = cb.call(cx, JSValue::UNDEFINED, &[obj]);
            }
            true
        },
        || true,
        || {},
    );

    Ok(JSValue::UNDEFINED)
}
