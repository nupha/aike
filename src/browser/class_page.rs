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
    browser::{cdp, input, network},
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
    FieldDef::FUNC(
        "waitForSelector",
        2,
        super::runtime::js_page_wait_for_selector,
        0,
    ),
    FieldDef::FUNC("evaluate", 1, super::runtime::js_page_evaluate, 0),
    FieldDef::FUNC("getOuterHTML", 0, super::runtime::js_page_get_outer_html, 0),
    FieldDef::FUNC("captureScreenshot", 1, js_page_capture_screenshot, 0),
    FieldDef::FUNC("setViewport", 2, js_page_set_viewport, 0),
    FieldDef::FUNC("waitForNavigation", 1, js_page_wait_for_navigation, 0),
    FieldDef::FUNC("handleDialog", 2, js_page_handle_dialog, 0),
    FieldDef::FUNC("printToPDF", 1, js_page_print_to_pdf, 0),
    FieldDef::FUNC("uploadFile", 2, js_page_upload_file, 0),
    FieldDef::FUNC("getLayoutMetrics", 0, js_page_get_layout_metrics, 0),
    FieldDef::FUNC("addInitScript", 1, js_page_add_init_script, 0),
    FieldDef::FUNC("getFrameTree", 0, js_page_get_frame_tree, 0),
    FieldDef::FUNC(
        "enableRuntime",
        0,
        super::runtime::js_page_enable_runtime,
        0,
    ),
    FieldDef::FUNC("close", 0, js_page_close, 0),
    // Page domain — extended
    FieldDef::FUNC("setContent", 1, js_page_set_content, 0),
    FieldDef::FUNC("reload", 1, js_page_reload, 0),
    FieldDef::FUNC("stopLoading", 0, js_page_stop_loading, 0),
    FieldDef::FUNC("back", 0, js_page_back, 0),
    FieldDef::FUNC("forward", 0, js_page_forward, 0),
    FieldDef::FUNC("getNavigationHistory", 0, js_page_get_navigation_history, 0),
    FieldDef::FUNC("bringToFront", 0, js_page_bring_to_front, 0),
    FieldDef::FUNC("captureSnapshot", 1, js_page_capture_snapshot, 0),
    FieldDef::FUNC("setDownloadBehavior", 1, js_page_set_download_behavior, 0),
    FieldDef::FUNC("setBypassCSP", 1, js_page_set_bypass_csp, 0),
    // Input domain
    FieldDef::FUNC("mouseClick", 4, input::js_page_mouse_click, 0),
    FieldDef::FUNC("mouseMove", 2, input::js_page_mouse_move, 0),
    FieldDef::FUNC("keyPress", 2, input::js_page_key_press, 0),
    FieldDef::FUNC("insertText", 1, input::js_page_insert_text, 0),
    // DOM domain
    FieldDef::FUNC("querySelector", 1, super::dom::js_page_query_selector, 0),
    FieldDef::FUNC(
        "querySelectorAll",
        1,
        super::dom::js_page_query_selector_all,
        0,
    ),
    FieldDef::FUNC("getAttribute", 2, super::dom::js_page_get_attribute, 0),
    FieldDef::FUNC("focus", 1, super::dom::js_page_focus, 0),
    FieldDef::FUNC("getBoxModel", 1, super::dom::js_page_get_box_model, 0),
    FieldDef::FUNC("remove", 1, super::dom::js_page_remove, 0),
    FieldDef::FUNC("click", 1, super::dom::js_page_click, 0),
    FieldDef::FUNC("type", 2, super::dom::js_page_type, 0),
    // Emulation domain
    FieldDef::FUNC(
        "setGeolocation",
        3,
        super::emulation::js_page_set_geolocation,
        0,
    ),
    FieldDef::FUNC(
        "setMediaFeatures",
        1,
        super::emulation::js_page_set_media_features,
        0,
    ),
    FieldDef::FUNC("setTimezone", 1, super::emulation::js_page_set_timezone, 0),
    FieldDef::FUNC("setLocale", 1, super::emulation::js_page_set_locale, 0),
    FieldDef::FUNC(
        "setTouchEnabled",
        1,
        super::emulation::js_page_set_touch_enabled,
        0,
    ),
    FieldDef::FUNC(
        "setCPUThrottling",
        1,
        super::emulation::js_page_set_cpu_throttling,
        0,
    ),
    // Fetch domain (request interception)
    FieldDef::FUNC(
        "setRequestInterception",
        1,
        super::fetch::js_page_set_request_interception,
        0,
    ),
    FieldDef::FUNC(
        "continueRequest",
        1,
        super::fetch::js_page_continue_request,
        0,
    ),
    FieldDef::FUNC("abortRequest", 1, super::fetch::js_page_abort_request, 0),
    FieldDef::FUNC(
        "respondRequest",
        1,
        super::fetch::js_page_respond_request,
        0,
    ),
    FieldDef::FUNC(
        "onRequestPaused",
        1,
        super::fetch::js_page_on_request_paused,
        0,
    ),
    // Network domain
    FieldDef::FUNC("getCookies", 0, network::js_page_get_cookies, 0),
    FieldDef::FUNC("setCookie", 1, network::js_page_set_cookie, 0),
    FieldDef::FUNC("getResponseBody", 1, network::js_page_get_response_body, 0),
    FieldDef::FUNC(
        "startNetworkCapture",
        0,
        network::js_page_start_network_capture,
        0,
    ),
    FieldDef::FUNC(
        "stopNetworkCapture",
        0,
        network::js_page_stop_network_capture,
        0,
    ),
    FieldDef::FUNC(
        "getCapturedRequests",
        0,
        network::js_page_get_captured_requests,
        0,
    ),
    FieldDef::FUNC(
        "clearCapturedRequests",
        0,
        network::js_page_clear_captured_requests,
        0,
    ),
    // Network domain — P0/P1 overrides
    FieldDef::FUNC("setUserAgent", 1, network::js_page_set_user_agent, 0),
    FieldDef::FUNC(
        "setExtraHTTPHeaders",
        1,
        network::js_page_set_extra_http_headers,
        0,
    ),
    FieldDef::FUNC("setOfflineMode", 1, network::js_page_set_offline_mode, 0),
    FieldDef::FUNC(
        "setCacheDisabled",
        1,
        network::js_page_set_cache_disabled,
        0,
    ),
    FieldDef::FUNC("setCookies", 1, network::js_page_set_cookies, 0),
    FieldDef::FUNC("deleteCookie", 2, network::js_page_delete_cookie, 0),
    FieldDef::FUNC(
        "clearBrowserCookies",
        0,
        network::js_page_clear_browser_cookies,
        0,
    ),
    // Log domain
    FieldDef::FUNC("onLog", 1, super::log::js_page_on_log, 0),
    FieldDef::FUNC(
        "getConsoleMessages",
        1,
        super::log::js_page_get_console_messages,
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

/// `page.navigate(url)`
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

/// `page.captureScreenshot(options?)`
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

/// `page.setViewport(width, height)`
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

/// `page.waitForNavigation(timeoutMs?)`
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

/// `page.handleDialog(accept, promptText?)`
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

/// `page.printToPDF(options?)`
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

/// `page.getLayoutMetrics()`
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

/// `page.addInitScript(source)`
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

/// `page.getFrameTree()`
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

// ---------------------------------------------------------------------------
// Page domain — extended methods
// ---------------------------------------------------------------------------

/// `page.setContent(html)` — replace the whole document via `Page.setDocumentContent`.
fn js_page_set_content(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let html = cc
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
        // Need the main frame id to set document content.
        let tree = client
            .send("Page.getFrameTree", None, Some(&session_id))
            .await
            .unwrap_or(Ok(serde_json::Value::Null));
        let frame_id = tree
            .as_ref()
            .ok()
            .and_then(|v| v.get("frameTree"))
            .and_then(|ft| ft.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match frame_id {
            Some(fid) => {
                let rx = client.send(
                    "Page.setDocumentContent",
                    Some(serde_json::json!({ "frameId": fid, "html": html })),
                    Some(&session_id),
                );
                let result = rx.await.unwrap_or(Err("channel closed".into()));
                match result {
                    Ok(_) => result_slot.commit(Ok(())).ok(),
                    Err(e) => result_slot.commit(Err(e)).ok(),
                };
            }
            None => {
                result_slot
                    .commit(Err("setContent: cannot resolve main frame".into()))
                    .ok();
            }
        }
    });

    Ok(promise.into())
}

/// `page.reload(opts?)` — `Page.reload`.
fn js_page_reload(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut params = serde_json::json!({});
    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        if let Some(v) = get_bool_prop(ctx, opts, "ignoreCache") {
            params["ignoreCache"] = Value::Bool(v);
        }
    }
    send_page_void_cmd(ctx, &cc, "Page.reload", Some(params))
}

/// `page.stopLoading()` — `Page.stopLoading`.
fn js_page_stop_loading(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Page.stopLoading", None)
}

/// `page.back()` — navigate to the previous history entry.
fn js_page_back(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    navigate_history(ctx, &cc, -1)
}

/// `page.forward()` — navigate to the next history entry.
fn js_page_forward(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    navigate_history(ctx, &cc, 1)
}

/// Shared helper: go `delta` steps in the navigation history via
/// `Page.getNavigationHistory` + `Page.navigateToHistoryEntry`.
fn navigate_history(ctx: &mut ScopedContext, cc: &CallContext, delta: i64) -> JSResult {
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
        let hist = client
            .send("Page.getNavigationHistory", None, Some(&session_id))
            .await
            .unwrap_or(Err("channel closed".into()));
        let (target_id, ok) = match hist {
            Ok(val) => {
                let current = val
                    .get("currentIndex")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let entries = val.get("entries").and_then(|v| v.as_array());
                let target = current + delta;
                let id = entries
                    .and_then(|arr| arr.get(target as usize))
                    .and_then(|e| e.get("id"))
                    .and_then(|v| v.as_i64());
                match id {
                    Some(i) => (i, true),
                    None => (0, false),
                }
            }
            Err(_) => (0, false),
        };

        if !ok {
            result_slot
                .commit(Err("no history entry in that direction".into()))
                .ok();
            return;
        }

        let rx = client.send(
            "Page.navigateToHistoryEntry",
            Some(serde_json::json!({ "entryId": target_id })),
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

/// `page.getNavigationHistory()` — returns `{ currentIndex, entries }`.
fn js_page_get_navigation_history(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
        let rx = client.send("Page.getNavigationHistory", None, Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// `page.bringToFront()` — `Page.bringToFront`.
fn js_page_bring_to_front(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    send_page_void_cmd(ctx, &cc, "Page.bringToFront", None)
}

/// `page.captureSnapshot(opts?)` — `Page.captureSnapshot` (MHTML).
fn js_page_capture_snapshot(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut params = serde_json::json!({ "format": "mhtml" });
    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        if let Some(f) = get_str_prop(ctx, opts, "format") {
            params["format"] = Value::String(f);
        }
    }

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
                .map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let rx = client.send("Page.captureSnapshot", Some(params), Some(&session_id));
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(val) => {
                let data = val
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                result_slot.commit(Ok(data)).ok();
            }
            Err(e) => {
                result_slot.commit(Err(e)).ok();
            }
        };
    });

    Ok(promise.into())
}

/// `page.setDownloadBehavior(path)` — `Page.setDownloadBehavior`.
/// Empty path → "deny", otherwise "allow" with the given download path.
fn js_page_set_download_behavior(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let params = if path.is_empty() {
        serde_json::json!({ "behavior": "deny" })
    } else {
        serde_json::json!({ "behavior": "allow", "downloadPath": path })
    };
    send_page_void_cmd(ctx, &cc, "Page.setDownloadBehavior", Some(params))
}

/// `page.setBypassCSP(enabled)` — `Page.setBypassCSP`.
fn js_page_set_bypass_csp(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let enabled = cc.arg(0).to_bool().unwrap_or(false);
    send_page_void_cmd(
        ctx,
        &cc,
        "Page.setBypassCSP",
        Some(serde_json::json!({ "enabled": enabled })),
    )
}

/// `page.uploadFile(selector, filePaths)`
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

/// `page.close()`
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
