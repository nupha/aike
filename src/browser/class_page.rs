// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — the main browser tab/page abstraction.

use std::{cell::Cell, sync::Arc, time::Duration};

use linkme::distributed_slice;
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use zuqe::{
    Array, CallContext, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
    builtins::typed_array::TypedArray,
    def_native_object,
};

use crate::{
    USER_CLASS_DEFS,
    browser::{cdp, chrome, class_input, class_network},
};

// ---------------------------------------------------------------------------
// Network capture state
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct PageNetworkState {
    pub(crate) capture_handle: Arc<AsyncMutex<Option<tokio::task::JoinHandle<()>>>>,
    pub(crate) captured_requests: Arc<AsyncMutex<Vec<Value>>>,
}

impl PageNetworkState {
    pub(crate) fn new() -> Self {
        Self {
            capture_handle: Arc::new(AsyncMutex::new(None)),
            captured_requests: Arc::new(AsyncMutex::new(Vec::new())),
        }
    }
}

// ---------------------------------------------------------------------------
// Payload
// ---------------------------------------------------------------------------

pub(crate) struct PagePayload {
    pub(crate) client: cdp::CdpClient,
    pub(crate) session_id: String,
    pub(crate) target_id: String,
    pub(crate) network: PageNetworkState,
}

// ---------------------------------------------------------------------------
// Proto table — ALL Page method entries
// ---------------------------------------------------------------------------

const PAGE_PROTO: &[FieldDef] = &[
    // Page domain
    FieldDef::FUNC("navigate", 1, js_page_navigate, 0),
    FieldDef::FUNC("waitForSelector", 2, js_page_wait_for_selector, 0),
    FieldDef::FUNC("evaluate", 1, js_page_evaluate, 0),
    FieldDef::FUNC("getOuterHTML", 0, js_page_get_outer_html, 0),
    FieldDef::FUNC("captureScreenshot", 1, js_page_capture_screenshot, 0),
    FieldDef::FUNC("setViewport", 2, js_page_set_viewport, 0),
    FieldDef::FUNC("waitForNavigation", 1, js_page_wait_for_navigation, 0),
    FieldDef::FUNC("handleDialog", 2, js_page_handle_dialog, 0),
    FieldDef::FUNC("printToPDF", 1, js_page_print_to_pdf, 0),
    FieldDef::FUNC("uploadFile", 2, js_page_upload_file, 0),
    FieldDef::FUNC("getLayoutMetrics", 0, js_page_get_layout_metrics, 0),
    FieldDef::FUNC("addInitScript", 1, js_page_add_init_script, 0),
    FieldDef::FUNC("getFrameTree", 0, js_page_get_frame_tree, 0),
    FieldDef::FUNC("enableRuntime", 0, js_page_enable_runtime, 0),
    FieldDef::FUNC("close", 0, js_page_close, 0),
    // Input domain
    FieldDef::FUNC("mouseClick", 4, class_input::js_page_mouse_click, 0),
    FieldDef::FUNC("mouseMove", 2, class_input::js_page_mouse_move, 0),
    FieldDef::FUNC("keyPress", 2, class_input::js_page_key_press, 0),
    FieldDef::FUNC("insertText", 1, class_input::js_page_insert_text, 0),
    // Network domain
    FieldDef::FUNC("getCookies", 0, class_network::js_page_get_cookies, 0),
    FieldDef::FUNC("setCookie", 1, class_network::js_page_set_cookie, 0),
    FieldDef::FUNC(
        "getResponseBody",
        1,
        class_network::js_page_get_response_body,
        0,
    ),
    FieldDef::FUNC(
        "startNetworkCapture",
        0,
        class_network::js_page_start_network_capture,
        0,
    ),
    FieldDef::FUNC(
        "stopNetworkCapture",
        0,
        class_network::js_page_stop_network_capture,
        0,
    ),
    FieldDef::FUNC(
        "getCapturedRequests",
        0,
        class_network::js_page_get_captured_requests,
        0,
    ),
    FieldDef::FUNC(
        "clearCapturedRequests",
        0,
        class_network::js_page_clear_captured_requests,
        0,
    ),
];

def_native_object!("Page", JSPage, PagePayload, PAGE_PROTO, None);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_page(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSPage::register(rt)
}

// ---------------------------------------------------------------------------
// Payload accessor
// ---------------------------------------------------------------------------

pub(crate) fn get_page_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a PagePayload, i32> {
    val.as_native_object::<JSPage>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not a Page instance");
            -1
        })
}

// ---------------------------------------------------------------------------
// Shared helper: void-returning CDP command
// ---------------------------------------------------------------------------

pub(crate) fn send_page_void_cmd(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    method: &'static str,
    params: Option<Value>,
) -> JSResult {
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
        let rx = client.send(method, params, Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Utility helpers
// ---------------------------------------------------------------------------

pub(crate) fn get_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<String> {
    let v = obj.get_property_str(ctx, key).ok()?;
    let s = v.as_string()?;
    Some(s.content().encode_utf8(false).to_string())
}

pub(crate) fn get_number_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<f64> {
    let v = obj.get_property_str(ctx, key).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_number(ctx).ok().map(|n| n.to_f64())
}

pub(crate) fn get_bool_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<bool> {
    let v = obj.get_property_str(ctx, key).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    Some(v.to_bool().unwrap_or(false))
}

/// Convert a `serde_json::Value` to a JS value.
pub(crate) fn js_from_json_value(ctx: &mut ScopedContext, val: &Value) -> JSResult {
    match val {
        Value::Null => Ok(JSValue::NULL),
        Value::Bool(b) => Ok(JSValue::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(JSValue::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(JSValue::from(f))
            } else {
                Ok(JSValue::from(0))
            }
        }
        Value::String(s) => ctx
            .create_string_from_utf8(s.as_bytes().to_vec())
            .map(JSValue::from),
        Value::Array(arr) => {
            let mut items = Vec::with_capacity(arr.len());
            for item in arr {
                match js_from_json_value(ctx, item) {
                    Ok(v) => items.push(v),
                    Err(_) => items.push(JSValue::NULL),
                }
            }
            Array::from_values(ctx, items).map(JSValue::from)
        }
        Value::Object(map) => {
            let mut obj = ctx.create_base_object()?;
            for (k, v) in map {
                let js_v = js_from_json_value(ctx, v)?;
                obj.define_property_value_str(ctx, k, js_v, zuqe::JS_PROP_C_W_E)?;
            }
            Ok(obj.into())
        }
    }
}

// ---------------------------------------------------------------------------
// Page methods — Page domain
// ---------------------------------------------------------------------------

/// `tab.navigate(url)`
fn js_page_navigate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let url = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    send_page_void_cmd(
        ctx,
        &cc,
        "Page.navigate",
        Some(serde_json::json!({ "url": url })),
    )
}

/// `tab.waitForSelector(selector, timeoutMs?)`
fn js_page_wait_for_selector(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

/// `tab.evaluate(jsCode)`
fn js_page_evaluate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

/// `tab.getOuterHTML()`
fn js_page_get_outer_html(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

/// `tab.captureScreenshot(options?)`
fn js_page_capture_screenshot(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut format = "png".to_string();
    let mut quality = None::<i32>;

    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        if let Some(f) = get_str_prop(ctx, opts, "format") {
            format = f;
        }
        if let Some(q) = get_number_prop(ctx, opts, "quality") {
            quality = Some(q as i32);
        }
    }

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let data = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, data).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let mut params = serde_json::json!({ "format": format });
        if let Some(q) = quality {
            params["quality"] = serde_json::json!(q);
        }
        let rx = client.send("Page.captureScreenshot", Some(params), Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                if let Some(b64) = val.get("data").and_then(|v| v.as_str()) {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap_or_default();
                    result_slot.commit(Ok(bytes)).ok();
                } else {
                    result_slot.commit(Err("no screenshot data".into())).ok();
                }
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `tab.setViewport(width, height)`
fn js_page_set_viewport(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let width = cc
        .arg(0)
        .to_number(ctx)
        .map(|n| n.to_i32())
        .unwrap_or(1280)
        .max(1);
    let height = cc
        .arg(1)
        .to_number(ctx)
        .map(|n| n.to_i32())
        .unwrap_or(720)
        .max(1);

    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setDeviceMetricsOverride",
        Some(serde_json::json!({
            "width": width,
            "height": height,
            "deviceScaleFactor": 1,
            "mobile": false,
        })),
    )
}

/// `tab.waitForNavigation(timeoutMs?)`
fn js_page_wait_for_navigation(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let timeout_ms: u64 = if cc.length() > 0 {
        let n = cc
            .arg(0)
            .to_number(ctx)
            .map(|n| n.to_i32())
            .unwrap_or(30000);
        n.max(1000) as u64
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
        // Enable Page domain to receive events
        let _ = client.send("Page.enable", None, Some(&session_id)).await;

        let mut event_rx = client.subscribe_events();
        let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    match event {
                        Ok(evt) => {
                            if evt.method == "Page.loadEventFired"
                                && evt.session_id.as_deref() == Some(&session_id)
                            {
                                result_slot.commit(Ok(())).ok();
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            result_slot.commit(Err("CDP connection closed".into())).ok();
                            break;
                        }
                    }
                }
                _ = &mut timeout => {
                    result_slot.commit(Err("navigation timeout".into())).ok();
                    break;
                }
            }
        }
    });

    Ok(promise.into())
}

/// `tab.handleDialog(accept, promptText?)`
fn js_page_handle_dialog(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let accept = cc.arg(0).to_bool().unwrap_or(false);
    let prompt_text = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        cc.arg(1)
            .to_string(ctx)
            .map(|s| s.content().encode_utf8(false).to_string())
            .ok()
    } else {
        None
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
        let mut params = serde_json::json!({ "accept": accept });
        if let Some(text) = prompt_text {
            params["promptText"] = Value::String(text);
        }
        let rx = client.send(
            "Page.handleJavaScriptDialog",
            Some(params),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

/// `tab.printToPDF(options?)`
fn js_page_print_to_pdf(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut params = serde_json::json!({ "printBackground": true });

    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        if let Some(v) = get_bool_prop(ctx, opts, "landscape") {
            params["landscape"] = Value::Bool(v);
        }
        if let Some(v) = get_number_prop(ctx, opts, "paperWidth") {
            params["paperWidth"] = serde_json::json!(v);
        }
        if let Some(v) = get_number_prop(ctx, opts, "paperHeight") {
            params["paperHeight"] = serde_json::json!(v);
        }
        if let Some(v) = get_bool_prop(ctx, opts, "printBackground") {
            params["printBackground"] = Value::Bool(v);
        }
    }

    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    let result_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let data = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, data).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let rx = client.send("Page.printToPDF", Some(params), Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                if let Some(b64) = val.get("data").and_then(|v| v.as_str()) {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .unwrap_or_default();
                    result_slot.commit(Ok(bytes)).ok();
                } else {
                    result_slot.commit(Err("no PDF data".into())).ok();
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
// Native CDP Page methods
// ---------------------------------------------------------------------------

/// `tab.getLayoutMetrics()`
fn js_page_get_layout_metrics(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
        let rx = client.send("Page.getLayoutMetrics", None, Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// `tab.addInitScript(source)`
fn js_page_add_init_script(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let source = cc
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
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let id = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(id.into_bytes())
                .map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let rx = client.send(
            "Page.addScriptToEvaluateOnNewDocument",
            Some(serde_json::json!({ "source": source })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let id = val
                    .get("identifier")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = result_slot.commit(Ok(id));
            }
            Err(e) => {
                let _ = result_slot.commit(Err(e));
            }
        }
    });

    Ok(promise.into())
}

/// `tab.getFrameTree()`
fn js_page_get_frame_tree(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
        let rx = client.send("Page.getFrameTree", None, Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// `tab.enableRuntime()`
fn js_page_enable_runtime(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Runtime.enable", None)
}

/// `tab.uploadFile(selector, filePaths)`
fn js_page_upload_file(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let mut file_paths: Vec<String> = Vec::new();
    if cc.length() > 1
        && let Some(arr) = cc.arg(1).as_object_Array()
    {
        for v in arr.payload().values() {
            if let Some(s) = v.as_string() {
                file_paths.push(s.content().encode_utf8(false).to_string());
            }
        }
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
        let set_files_js = file_paths
            .iter()
            .map(|p| {
                format!(
                    "dt.items.add(new File([new Blob()], '{}'))",
                    p.replace('\'', "\\'")
                )
            })
            .collect::<Vec<_>>()
            .join(";\n");

        let js_code = format!(
            "(() => {{\
             const input = document.querySelector('{}');\
             if (!input) throw new Error('input not found');\
             const dt = new DataTransfer();\
             {};\
             input.files = dt.files;\
             input.dispatchEvent(new Event('change', {{ bubbles: true }}));\
             }})()",
            selector.replace('\'', "\\'"),
            set_files_js,
        );

        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({ "expression": js_code })),
            Some(&session_id),
        );
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                if val.get("exceptionDetails").is_some() {
                    result_slot.commit(Err("upload: JS exception".into())).ok();
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

/// `tab.close()`
fn js_page_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let target_id = payload.target_id.clone();

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
        let result = client.close_target(&target_id).await;
        match result {
            Ok(()) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Browser class — the top-level browser instance
// ---------------------------------------------------------------------------

/// Data passed from async launch task to JS callback.
pub(crate) struct LaunchData {
    pub(crate) client: cdp::CdpClient,
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) reader_handle: Option<tokio::task::JoinHandle<()>>,
}

struct BrowserPayload {
    client: cdp::CdpClient,
    chrome_child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
}

// ---------------------------------------------------------------------------
// Module-level: launch()
// ---------------------------------------------------------------------------

/// `launch(options?)` → Promise<Browser>
pub(crate) fn js_launch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut headless = true;
    let mut port = 9222u16;
    let mut proxy_url: Option<String> = None;
    let mut chrome_path: Option<String> = None;

    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        proxy_url = get_str_prop(ctx, opts, "proxyUrl");
        chrome_path = get_str_prop(ctx, opts, "chromePath");
        if let Some(v) = get_number_prop(ctx, opts, "port") {
            port = v as u16;
        }
        let hv = opts
            .get_property_str(ctx, "headless")
            .unwrap_or(JSValue::UNDEFINED);
        if !hv.is_undefined() {
            headless = hv.to_bool().unwrap_or(false);
        }
    }

    let result_slot = PromiseResultSlot::<Result<LaunchData, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let data = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

            let obj = JSBrowser::new_instance(
                cx,
                JSValue::UNDEFINED,
                BrowserPayload {
                    client: data.client,
                    chrome_child: Arc::new(tokio::sync::Mutex::new(data.child)),
                },
            )?;

            Ok(obj.into())
        })?;

    tokio::spawn(async move {
        let result =
            chrome::launch(headless, port, proxy_url.as_deref(), chrome_path.as_deref()).await;

        let (mut chrome_child, ws_url) = match result {
            Ok(r) => r,
            Err(e) => {
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        let (client, reader_handle) = match cdp::CdpClient::connect(&ws_url).await {
            Ok(r) => r,
            Err(e) => {
                let _ = chrome_child.start_kill();
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        let data = LaunchData {
            client,
            child: Some(chrome_child),
            reader_handle: Some(reader_handle),
        };
        result_slot.commit(Ok(data)).ok();
    });

    Ok(promise.into())
}

// ---------------------------------------------------------------------------
// Browser class
// ---------------------------------------------------------------------------

const BROWSER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("newPage", 1, js_browser_new_page, 0),
    FieldDef::FUNC("close", 0, js_browser_close, 0),
    FieldDef::GETSET("isRunning", Some(js_browser_is_running), None, 0),
];

def_native_object!("Browser", JSBrowser, BrowserPayload, BROWSER_PROTO, None);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_browser(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSBrowser::register(rt)
}

fn get_browser_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a BrowserPayload, i32> {
    val.as_native_object::<JSBrowser>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not a Browser instance");
            -1
        })
}

/// `browser.newTab()` → Promise<Page>
fn js_browser_new_page(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();
    let client_cb = client.clone();

    let result_slot = PromiseResultSlot::<Result<(String, String), String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, payload| -> JSResult {
            let (target_id, session_id) = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

            let mut page = JSPage::new_instance(
                cx,
                JSValue::UNDEFINED,
                PagePayload {
                    client: client_cb.clone(),
                    session_id,
                    target_id: target_id.clone(),
                    network: PageNetworkState::new(),
                },
            )?;

            // Set targetId readonly property
            let js_id = cx.create_string_from_utf8(target_id.into_bytes())?;
            page.define_property_value_str(cx, "targetId", js_id.into(), zuqe::JS_PROP_C_W_E)?;
            Ok(page.into())
        },
    )?;

    tokio::spawn(async move {
        let result = client.create_target("about:blank").await;
        match result {
            Ok((tid, sid)) => {
                result_slot.commit(Ok((tid, sid))).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        }
    });

    Ok(promise.into())
}

/// `browser.close()`
fn js_browser_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();
    let chrome_child = payload.chrome_child.clone();

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
        let rx = client.send("Browser.close", None, None);
        let _ = rx.await;
        // Kill the Chrome process
        if let Ok(mut guard) = chrome_child.try_lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.start_kill();
            }
            *guard = None;
        }
        result_slot.commit(Ok(())).ok();
    });

    Ok(promise.into())
}

/// `browser.isRunning` getter
fn js_browser_is_running(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let guard = payload.chrome_child.try_lock();
    match guard {
        Ok(inner) => Ok(JSValue::Bool(inner.is_some())),
        Err(_) => Ok(JSValue::Bool(false)),
    }
}
