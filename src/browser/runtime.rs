// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Runtime domain methods — flat on `Page`
//! (`page.evaluate` / `page.getOuterHTML` / `page.waitForSelector` /
//! `page.enableRuntime`). The underlying CDP commands are `Runtime.*`.
//!
//! Shared Page plumbing (`get_page_payload`, `send_page_void_cmd`) lives in
//! `class_page.rs` and is imported here. JSON→JS conversion uses zuqe's
//! `JSValue::from_serde_json`.

use crossbeam::channel;
use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::class_page::{get_page_payload, send_page_void_cmd};

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

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;

            if let Some(exception) = val.get("exceptionDetails") {
                let text = exception
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("JS exception");
                return Err(cx.throw_internal_error(text));
            }

            if let Some(result) = val.get("result") {
                if let Some(value) = result.get("value") {
                    return JSValue::from_serde_json(cx, value);
                }
                if result.get("objectId").is_some()
                    && let Some(desc) = result.get("description").and_then(|v| v.as_str())
                {
                    return cx.create_string_from_str(desc).map(JSValue::make_string);
                }
                if let Some(desc) = result.get("description").and_then(|v| v.as_str()) {
                    return cx.create_string_from_str(desc).map(JSValue::make_string);
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
            let html = payload.map_err(|e| cx.throw_internal_error(&e))?;
            cx.create_string_from_utf8(html.into_bytes())
                .map(JSValue::make_string)
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
            payload.map_err(|e| cx.throw_internal_error(&e))?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        // Retry with a short per-attempt timeout so that, if `navigate` returns
        // before the new document is ready, the evaluate that ran against a
        // stale execution context is retried against the fresh one once the
        // navigation commits.
        let start = std::time::Instant::now();
        let overall = std::time::Duration::from_millis(timeout_ms.max(0) as u64);
        loop {
            let remaining = overall.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                result_slot
                    .commit(Err("selector not found or timed out".into()))
                    .ok();
                return;
            }
            let attempt_ms = remaining.as_millis().min(1000) as i64;

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
                attempt_ms
            );

            let rx = client.send(
                "Runtime.evaluate",
                Some(serde_json::json!({
                    "expression": js_code,
                    "awaitPromise": true,
                })),
                Some(&session_id),
            );
            match rx.await {
                Ok(Ok(val)) => {
                    // exceptionDetails (in-page timeout/reject) or a CDP error
                    // (e.g. stale execution context) → retry against a fresh
                    // document.
                    if val.get("exceptionDetails").is_some() || val.get("error").is_some() {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                    result_slot.commit(Ok(())).ok();
                    return;
                }
                Ok(Err(_)) | Err(_) => {
                    result_slot.commit(Err("channel closed".into())).ok();
                    return;
                }
            }
        }
    });

    Ok(promise.into())
}

/// `page.enableRuntime()`
pub(crate) fn js_page_enable_runtime(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Runtime.enable", None)
}

/// `page.waitForFunction(expression, timeoutMs?)`
/// Poll `expression` (any JS) every 100ms until it evaluates truthy or timeout.
/// Mirrors `page.waitForSelector` but for arbitrary conditions / text.
pub(crate) fn js_page_wait_for_function(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let expr = cc
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
            payload.map_err(|e| cx.throw_internal_error(&e))?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let mut js = String::from(
            "new Promise((resolve, reject) => {\
            const deadline = Date.now() + ",
        );
        js.push_str(&timeout_ms.to_string());
        js.push_str(
            ";\
            const tick = () => {\
                let ok = false;\
                try { ok = !!(( ",
        );
        js.push_str(&expr);
        js.push_str(
            " )); } catch (e) { ok = false; }\
                if (ok) { resolve(true); return; }\
                if (Date.now() > deadline) { reject(new Error('timeout')); return; }\
                setTimeout(tick, 100);\
            };\
            tick();\
        })",
        );
        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({ "expression": js, "awaitPromise": true })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                if val.get("exceptionDetails").is_some() {
                    result_slot
                        .commit(Err("waitForFunction: condition not met or timed out".into()))
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

// ---------------------------------------------------------------------------
// Runtime domain
// ---------------------------------------------------------------------------

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
            call_args.push(serde_json::json!({ "value": item.to_serde_json_primitive(ctx) }));
        }
    }

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
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
                    result_slot
                        .commit(Err("callFunctionOn: JS exception".into()))
                        .ok();
                } else {
                    let r = val
                        .get("result")
                        .and_then(|x| x.get("value"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
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

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
        })?;

    tokio::spawn(async move {
        let props_rx = client.send(
            "Runtime.getProperties",
            Some(serde_json::json!({ "objectId": object_id, "ownProperties": true })),
            Some(&session_id),
        );
        let props = match props_rx.await.unwrap_or(Err("channel closed".into())) {
            Ok(val) => val
                .get("result")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default(),
            Err(e) => {
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        // Build a name -> value map. Primitive values come inline via `value`;
        // object/array values only carry an `objectId`, so resolve them by value
        // with a callFunctionOn round-trip (handles nested structures).
        let mut map = serde_json::Map::new();
        for p in &props {
            let Some(name) = p.get("name").and_then(|x| x.as_str()) else {
                continue;
            };
            let remote = p.get("value");
            let resolved = if let Some(v) = remote.and_then(|x| x.get("value")) {
                v.clone()
            } else if let Some(oid) = remote
                .and_then(|x| x.get("objectId"))
                .and_then(|x| x.as_str())
            {
                let r = client
                    .send(
                        "Runtime.callFunctionOn",
                        Some(serde_json::json!({
                            "objectId": oid,
                            "functionDeclaration": "function () { return this; }",
                            "returnByValue": true,
                        })),
                        Some(&session_id),
                    )
                    .await
                    .unwrap_or(Ok(serde_json::Value::Null));
                match r {
                    Ok(j) => j
                        .get("result")
                        .and_then(|x| x.get("value"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    Err(_) => serde_json::Value::Null,
                }
            } else {
                serde_json::Value::Null
            };
            map.insert(name.to_string(), resolved);
        }
        result_slot.commit(Ok(serde_json::Value::Object(map))).ok();
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

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, val| -> JSResult {
            let val = val.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
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
    pub(crate) static CONSOLE_CALLBACK: std::cell::RefCell<std::collections::HashMap<String, JSValue>> =
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

    let (tx, rx) = ctx.make_wake_channel::<serde_json::Value>();
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
