// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Network domain methods.
//!
//! Cookies, network request capture, response body retrieval.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use zuqe::{
    Array, CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::{
    cdp_network,
    class_page::{get_bool_prop, get_page_payload, get_str_prop, send_page_void_cmd},
};

/// Convert a CDP cookie JSON value to a JS object.
fn cookie_to_js(ctx: &mut ScopedContext, c: &serde_json::Value) -> Result<JSValue, i32> {
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
            obj.define_property_value_str(
                ctx,
                key,
                JSValue::make_string(js_s),
                zuqe::JS_PROP_C_W_E,
            )?;
        }
    }
    if let Some(http_only) = c.get("httpOnly").and_then(|v| v.as_bool()) {
        obj.define_property_value_str(
            ctx,
            "httpOnly",
            JSValue::from(http_only),
            zuqe::JS_PROP_C_W_E,
        )?;
    }
    if let Some(secure) = c.get("secure").and_then(|v| v.as_bool()) {
        obj.define_property_value_str(ctx, "secure", JSValue::from(secure), zuqe::JS_PROP_C_W_E)?;
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

    let result_slot = PromiseResultSlot::<Result<Vec<serde_json::Value>, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let cookies = payload.map_err(|e| cx.throw_internal_error(&e))?;

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
    let cookie_obj = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("setCookie requires an object argument"))?;

    let mut params = serde_json::Map::new();
    if let Some(name) = get_str_prop(ctx, cookie_obj, "name") {
        params.insert("name".into(), serde_json::Value::String(name));
    }
    if let Some(value) = get_str_prop(ctx, cookie_obj, "value") {
        params.insert("value".into(), serde_json::Value::String(value));
    }
    if let Some(domain) = get_str_prop(ctx, cookie_obj, "domain") {
        params.insert("domain".into(), serde_json::Value::String(domain));
    }
    if let Some(path) = get_str_prop(ctx, cookie_obj, "path") {
        params.insert("path".into(), serde_json::Value::String(path));
    }

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
        // Network.enable is a prerequisite for Network.setCookie
        let _ = cdp_network::enable_network(&client, &session_id).await;
        let rx = cdp_network::set_cookie(&client, &session_id, serde_json::Value::Object(params));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Network domain — P0/P1 overrides & helpers
// ---------------------------------------------------------------------------

/// Convert a zuqe plain object to a `serde_json::Value` (shallow).
fn net_obj_to_json(ctx: &mut ScopedContext, obj: &mut zuqe::Object) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let flags = zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK;
    if let Ok(keys) = obj.get_own_property_names(ctx, flags) {
        for pk in keys.iter() {
            let v = match obj.get_property(ctx, pk.atom) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let key = pk
                .atom
                .to_value(ctx)
                .ok()
                .and_then(|v| v.to_string(ctx).ok())
                .map(|s| s.content().encode_utf8(false).to_string())
                .unwrap_or_default();
            if let Some(s) = v.as_string(true) {
                map.insert(
                    key,
                    serde_json::Value::String(s.content().encode_utf8(false).to_string()),
                );
            } else if let Some(n) = v.to_number(ctx).ok() {
                map.insert(key, serde_json::Value::from(n.to_f64()));
            } else if v.is_bool() {
                map.insert(key, serde_json::Value::Bool(v.to_bool().unwrap_or(false)));
            }
        }
    }
    serde_json::Value::Object(map)
}

/// `page.setUserAgent(userAgent)` — `Network.setUserAgentOverride`.
pub(crate) fn js_page_set_user_agent(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let ua = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    send_page_void_cmd(
        ctx,
        &cc,
        "Network.setUserAgentOverride",
        Some(serde_json::json!({ "userAgent": ua })),
    )
}

/// `page.setExtraHTTPHeaders(headers)` — `Network.setExtraHTTPHeaders`.
pub(crate) fn js_page_set_extra_http_headers(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut headers = serde_json::Map::new();
    if cc.length() > 0
        && let Some(obj) = cc.arg(0).as_object()
    {
        let mut obj = obj.clone();
        let v = net_obj_to_json(ctx, &mut obj);
        if let serde_json::Value::Object(m) = v {
            headers = m;
        }
    }
    send_page_void_cmd(
        ctx,
        &cc,
        "Network.setExtraHTTPHeaders",
        Some(serde_json::json!({ "headers": headers })),
    )
}

/// `page.setOfflineMode(bool)` — `Network.emulateNetworkConditions`.
pub(crate) fn js_page_set_offline_mode(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let offline = cc.arg(0).to_bool().unwrap_or(false);
    send_page_void_cmd(
        ctx,
        &cc,
        "Network.emulateNetworkConditions",
        Some(serde_json::json!({
            "offline": offline,
            "latency": 0,
            "downloadThroughput": -1,
            "uploadThroughput": -1,
        })),
    )
}

/// `page.setCacheDisabled(bool)` — `Network.setCacheDisabled`.
pub(crate) fn js_page_set_cache_disabled(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let disabled = cc.arg(0).to_bool().unwrap_or(false);
    send_page_void_cmd(
        ctx,
        &cc,
        "Network.setCacheDisabled",
        Some(serde_json::json!({ "cacheDisabled": disabled })),
    )
}

/// `page.setCookies(cookies)` — `Network.setCookies` (batch).
pub(crate) fn js_page_set_cookies(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut cookies = Vec::new();
    if cc.length() > 0
        && let Some(arr) = cc.arg(0).as_object_Array()
    {
        for item in arr.payload().values() {
            if let Some(obj) = item.as_object() {
                let mut c = serde_json::Map::new();
                if let Some(name) = get_str_prop(ctx, obj, "name") {
                    c.insert("name".into(), serde_json::Value::String(name));
                }
                if let Some(value) = get_str_prop(ctx, obj, "value") {
                    c.insert("value".into(), serde_json::Value::String(value));
                }
                if let Some(domain) = get_str_prop(ctx, obj, "domain") {
                    c.insert("domain".into(), serde_json::Value::String(domain));
                }
                if let Some(path) = get_str_prop(ctx, obj, "path") {
                    c.insert("path".into(), serde_json::Value::String(path));
                }
                if let Some(secure) = get_bool_prop(ctx, obj, "secure") {
                    c.insert("secure".into(), serde_json::Value::Bool(secure));
                }
                if let Some(http_only) = get_bool_prop(ctx, obj, "httpOnly") {
                    c.insert("httpOnly".into(), serde_json::Value::Bool(http_only));
                }
                cookies.push(serde_json::Value::Object(c));
            }
        }
    }

    send_page_void_cmd(
        ctx,
        &cc,
        "Network.setCookies",
        Some(serde_json::json!({ "cookies": cookies })),
    )
}

/// `page.deleteCookie(name, url)` — `Network.deleteCookies`.
pub(crate) fn js_page_delete_cookie(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let name = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let url = cc
        .arg(1)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let mut params = serde_json::json!({ "name": name });
    if !url.is_empty() {
        params["url"] = serde_json::Value::String(url);
    }
    send_page_void_cmd(ctx, &cc, "Network.deleteCookies", Some(params))
}

/// `page.clearBrowserCookies()` — `Network.clearBrowserCookies`.
pub(crate) fn js_page_clear_browser_cookies(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Network.clearBrowserCookies", None)
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

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let val = payload.map_err(|e| cx.throw_internal_error(&e))?;
            JSValue::from_serde_json(cx, &val)
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
            payload.map_err(|e| cx.throw_internal_error(&e))?;
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
            payload.map_err(|e| cx.throw_internal_error(&e))?;
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
    let requests: Vec<serde_json::Value> = if let Ok(guard) = network.captured_requests.try_lock() {
        guard.clone()
    } else {
        Vec::new()
    };

    JSValue::from_serde_json(ctx, &serde_json::Value::Array(requests))
}

/// `page.getRequestPostData(requestId)` → POST body (string) via
/// `Network.getRequestPostData`. Requires the request to be intercepted
/// (Fetch domain) so the body is buffered.
pub(crate) fn js_page_get_request_post_data(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let request_id = cc
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
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, body| -> JSResult {
            let body = body.map_err(|e| cx.throw_internal_error(&e))?;
            cx.create_string_from_utf8(body.into_bytes())
                .map(JSValue::make_string)
        })?;

    tokio::spawn(async move {
        // Ensure the Network domain is enabled (idempotent) so the request's
        // post body is tracked.
        let _ = cdp_network::enable_network(&client, &session_id).await;
        let rx = client.send(
            "Network.getRequestPostData",
            Some(serde_json::json!({ "requestId": request_id })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let body = val
                    .get("postData")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                result_slot.commit(Ok(body)).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
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
