// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Network domain methods.
//!
//! Cookies, network request capture, response body retrieval.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde_json::Value;
use zuqe::{
    Array, CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::{
    cdp_network,
    class_page::{get_page_payload, get_str_prop, js_from_json_value},
};

/// Convert a CDP cookie JSON value to a JS object.
fn cookie_to_js(ctx: &mut ScopedContext, c: &Value) -> Result<JSValue, i32> {
    let mut obj = ctx.create_base_object()?;
    let props = [
        ("name", c.get("name").and_then(|v| v.as_str())),
        ("value", c.get("value").and_then(|v| v.as_str())),
        ("domain", c.get("domain").and_then(|v| v.as_str())),
        ("path", c.get("path").and_then(|v| v.as_str())),
    ];
    for (key, val) in &props {
        if let Some(s) = val {
            let js_s = ctx.create_string_from_utf8(s.as_bytes().to_vec())?;
            obj.define_property_value_str(ctx, key, js_s.into(), zuqe::JS_PROP_C_W_E)?;
        }
    }
    if let Some(http_only) = c.get("httpOnly").and_then(|v| v.as_bool()) {
        obj.define_property_value_str(
            ctx,
            "httpOnly",
            JSValue::Bool(http_only),
            zuqe::JS_PROP_C_W_E,
        )?;
    }
    if let Some(secure) = c.get("secure").and_then(|v| v.as_bool()) {
        obj.define_property_value_str(ctx, "secure", JSValue::Bool(secure), zuqe::JS_PROP_C_W_E)?;
    }
    Ok(obj.into())
}

// ---------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------

/// `page.getCookies()`
pub(crate) fn js_page_get_cookies(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<Vec<Value>, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let cookies = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

            let mut js_cookies: Vec<JSValue> = Vec::with_capacity(cookies.len());
            for c in &cookies {
                if let Ok(obj) = cookie_to_js(cx, c) {
                    js_cookies.push(obj);
                }
            }
            Array::from_values(cx, js_cookies).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let rx = cdp_network::get_all_cookies(&client, &session_id);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let cookies = val
                    .get("cookies")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                result_slot.commit(Ok(cookies)).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `page.setCookie(cookie)`
pub(crate) fn js_page_set_cookie(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let cookie_obj = cc.arg(0).as_object().copied().ok_or_else(|| {
        ctx.throw_type_error("setCookie requires an object argument");
        -1
    })?;

    let mut params = serde_json::Map::new();
    if let Some(name) = get_str_prop(ctx, &cookie_obj, "name") {
        params.insert("name".into(), Value::String(name));
    }
    if let Some(value) = get_str_prop(ctx, &cookie_obj, "value") {
        params.insert("value".into(), Value::String(value));
    }
    if let Some(domain) = get_str_prop(ctx, &cookie_obj, "domain") {
        params.insert("domain".into(), Value::String(domain));
    }
    if let Some(path) = get_str_prop(ctx, &cookie_obj, "path") {
        params.insert("path".into(), Value::String(path));
    }

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
        // Network.enable is a prerequisite for Network.setCookie
        let _ = cdp_network::enable_network(&client, &session_id).await;
        let rx = cdp_network::set_cookie(&client, &session_id, Value::Object(params));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Network capture
// ---------------------------------------------------------------------------

/// `page.getResponseBody(requestId)`
/// Returns `{ body: string, base64Encoded: boolean }`.
pub(crate) fn js_page_get_response_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let request_id = cc
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
            js_from_json_value(cx, &val)
        })?;

    tokio::spawn(async move {
        let rx = cdp_network::get_response_body(&client, &session_id, &request_id);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// `page.startNetworkCapture()`
/// Enables the Network domain and begins capturing request/response events.
/// Captured data can be retrieved via `getCapturedRequests()`.
pub(crate) fn js_page_start_network_capture(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();
    let network = payload.network.clone();

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
        // Enable Network domain
        let rx = cdp_network::enable_network(&client, &session_id);
        if let Err(e) = rx.await.unwrap_or(Err("channel closed".into())) {
            result_slot.commit(Err(e)).ok();
            return;
        }

        // Subscribe to CDP events and store captured requests
        let running = Arc::new(AtomicBool::new(true));
        let captured = network.captured_requests.clone();
        let mut event_rx = client.subscribe_events();
        let session_id_cap = session_id.clone();
        let running_clone = running.clone();

        let handle = tokio::spawn(async move {
            while running_clone.load(Ordering::SeqCst) {
                tokio::select! {
                    event = event_rx.recv() => {
                        match event {
                            Ok(evt) if evt.session_id.as_deref() == Some(&session_id_cap) => {
                                let is_network_event = evt.method.starts_with("Network.");
                                if is_network_event {
                                    let entry = serde_json::json!({
                                        "method": evt.method,
                                        "params": evt.params,
                                    });
                                    if let Ok(mut guard) = captured.try_lock() {
                                        guard.push(entry);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
                }
            }
        });

        // Store the handle
        if let Ok(mut guard) = network.capture_handle.try_lock() {
            *guard = Some(handle);
        }

        result_slot.commit(Ok(())).ok();
    });

    Ok(promise.into())
}

/// `page.stopNetworkCapture()`
/// Stops the network capture background task.
pub(crate) fn js_page_stop_network_capture(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let network = payload.network.clone();

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
        if let Ok(mut guard) = network.capture_handle.try_lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
        result_slot.commit(Ok(())).ok();
    });

    Ok(promise.into())
}

/// `page.getCapturedRequests()`
/// Returns the array of captured network events.
pub(crate) fn js_page_get_captured_requests(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let network = payload.network.clone();

    // Clone the captured requests synchronously
    let requests: Vec<Value> = if let Ok(guard) = network.captured_requests.try_lock() {
        guard.clone()
    } else {
        Vec::new()
    };

    js_from_json_value(ctx, &Value::Array(requests))
}

/// `page.clearCapturedRequests()`
pub(crate) fn js_page_clear_captured_requests(
    ctx: &mut ScopedContext,
    cc: CallContext,
) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let network = payload.network.clone();

    if let Ok(mut guard) = network.captured_requests.try_lock() {
        guard.clear();
    }

    Ok(JSValue::UNDEFINED)
}
