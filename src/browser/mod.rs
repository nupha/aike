// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! CDP browser automation module for Aike — drives Chrome via DevTools Protocol.
//!
//! Exposes:
//! - `launch(options?)`              — module-level function, returns Promise<Browser>
//! - `Browser` class (not constructable from JS)
//! - `Tab` class (not constructable from JS)

mod cdp;
mod chrome;

use std::cell::Cell;
use std::sync::Arc;

use linkme::distributed_slice;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use zuqe::{
    Array, CallContext, ClassID, FieldDef, JSResult, JSValue, Object, Runtime, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
    builtins::typed_array::TypedArray,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

// ---------------------------------------------------------------------------
// Data passed from async tasks to JS callbacks
// ---------------------------------------------------------------------------

/// Result of launching Chrome — passed through PromiseResultSlot to the
/// JS callback which creates the `Browser` native object.
pub(crate) struct LaunchData {
    pub(crate) client: cdp::CdpClient,
    pub(crate) child: Option<tokio::process::Child>,
    pub(crate) reader_handle: Option<tokio::task::JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// Payload types for native objects
// ---------------------------------------------------------------------------

struct BrowserPayload {
    client: cdp::CdpClient,
    chrome_child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
}

struct TabPayload {
    client: cdp::CdpClient,
    session_id: String,
    target_id: String,
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

const MOD_BROWSER_ENTRIES: &[FieldDef] = &[FieldDef::FUNC("launch", 1, js_launch, 0)];

pub fn init_mod_browser(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("browser", MOD_BROWSER_ENTRIES)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Module-level: launch()
// ---------------------------------------------------------------------------

/// `launch(options?)` → Promise<Browser>
fn js_launch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut headless = true;
    let mut port = 9222u16;
    let mut proxy_url: Option<String> = None;
    let mut chrome_path: Option<String> = None;

    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
            proxy_url = get_str_prop(ctx, opts, "proxyUrl");
            chrome_path = get_str_prop(ctx, opts, "chromePath");
            if let Some(v) = get_number_prop(ctx, opts, "port") {
                port = v as u16;
            }
            let hv = opts
                .get_property_str(ctx, "headless")
                .unwrap_or(JSValue::UNDEFINED);
            if !hv.is_undefined() {
                // JSValue: truthy check via to_bool
                headless = hv.to_bool().unwrap_or(false);
            }
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
                    chrome_child: Arc::new(AsyncMutex::new(data.child)),
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
    FieldDef::FUNC("newTab", 1, js_browser_new_tab, 0),
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

/// `browser.newTab()` → Promise<Tab>
fn js_browser_new_tab(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

            let mut tab = JSTab::new_instance(
                cx,
                JSValue::UNDEFINED,
                TabPayload {
                    client: client_cb.clone(),
                    session_id,
                    target_id: target_id.clone(),
                },
            )?;

            // Set targetId readonly property
            let js_id = cx.create_string_from_utf8(target_id.into_bytes())?;
            tab.define_property_value_str(cx, "targetId", js_id.into(), zuqe::JS_PROP_C_W_E)?;
            Ok(tab.into())
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

// ---------------------------------------------------------------------------
// Tab class
// ---------------------------------------------------------------------------

const TAB_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("navigate", 1, js_tab_navigate, 0),
    FieldDef::FUNC("waitForSelector", 2, js_tab_wait_for_selector, 0),
    FieldDef::FUNC("evaluate", 1, js_tab_evaluate, 0),
    FieldDef::FUNC("getOuterHTML", 0, js_tab_get_outer_html, 0),
    FieldDef::FUNC("captureScreenshot", 1, js_tab_capture_screenshot, 0),
    FieldDef::FUNC("setViewport", 2, js_tab_set_viewport, 0),
    FieldDef::FUNC("getCookies", 0, js_tab_get_cookies, 0),
    FieldDef::FUNC("setCookie", 1, js_tab_set_cookie, 0),
    FieldDef::FUNC("waitForNavigation", 1, js_tab_wait_for_navigation, 0),
    FieldDef::FUNC("handleDialog", 2, js_tab_handle_dialog, 0),
    FieldDef::FUNC("printToPDF", 1, js_tab_print_to_pdf, 0),
    FieldDef::FUNC("uploadFile", 2, js_tab_upload_file, 0),
    FieldDef::FUNC("close", 0, js_tab_close, 0),
];

def_native_object!("Tab", JSTab, TabPayload, TAB_PROTO, None);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_tab(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSTab::register(rt)
}

fn get_tab_payload<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a TabPayload, i32> {
    val.as_native_object::<JSTab>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not a Tab instance");
            -1
        })
}

// ---------------------------------------------------------------------------
// Tab methods
// ---------------------------------------------------------------------------

/// `tab.navigate(url)`
fn js_tab_navigate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let url = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    send_tab_void_cmd(
        ctx,
        &cc,
        "Page.navigate",
        Some(serde_json::json!({ "url": url })),
    )
}

/// `tab.waitForSelector(selector, timeoutMs?)`
fn js_tab_wait_for_selector(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
    let payload = get_tab_payload(ctx, &this)?;
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
fn js_tab_evaluate(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let js_code = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
                if result.get("objectId").is_some() {
                    // Object returned without a value (e.g. too large); use description as fallback
                    if let Some(desc) = result.get("description").and_then(|v| v.as_str()) {
                        return cx
                            .create_string_from_utf8(desc.as_bytes().to_vec())
                            .map(JSValue::from);
                    }
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
fn js_tab_get_outer_html(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
fn js_tab_capture_screenshot(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut format = "png".to_string();
    let mut quality = None::<i32>;

    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
            if let Some(f) = get_str_prop(ctx, opts, "format") {
                format = f;
            }
            if let Some(q) = get_number_prop(ctx, opts, "quality") {
                quality = Some(q as i32);
            }
        }
    }

    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
fn js_tab_set_viewport(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

    send_tab_void_cmd(
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

/// `tab.getCookies()`
fn js_tab_get_cookies(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
        let rx = client.send("Network.getAllCookies", None, Some(&session_id));
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

/// `tab.setCookie(cookie)`
fn js_tab_set_cookie(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
    let payload = get_tab_payload(ctx, &this)?;
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
        let _ = client.send("Network.enable", None, Some(&session_id)).await;
        let rx = client.send(
            "Network.setCookie",
            Some(Value::Object(params)),
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

/// `tab.waitForNavigation(timeoutMs?)`
fn js_tab_wait_for_navigation(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
        let _ = client.send("Page.enable", None, Some(&session_id)).await;
        let rx = client.send(
            "Runtime.evaluate",
            Some(serde_json::json!({
                "expression": "new Promise(resolve => { \
                    if (document.readyState === 'complete') { resolve(true); return; } \
                    window.addEventListener('load', () => resolve(true), { once: true }); \
                })",
                "awaitPromise": true,
            })),
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

/// `tab.handleDialog(accept, promptText?)`
fn js_tab_handle_dialog(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
    let payload = get_tab_payload(ctx, &this)?;
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
fn js_tab_print_to_pdf(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut params = serde_json::json!({ "printBackground": true });

    if cc.length() > 0 {
        if let Some(opts) = cc.arg(0).as_object() {
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
    }

    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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

/// `tab.uploadFile(selector, filePaths)`
fn js_tab_upload_file(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let selector = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let mut file_paths: Vec<String> = Vec::new();
    if cc.length() > 1 {
        if let Some(arr) = cc.arg(1).as_object_Array() {
            for v in arr.payload().values() {
                if let Some(s) = v.as_string() {
                    file_paths.push(s.content().encode_utf8(false).to_string());
                }
            }
        }
    }

    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
        // Use JS to set files via DataTransfer API
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
fn js_tab_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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
// Shared helper: send a void-returning CDP command on a tab
// ---------------------------------------------------------------------------

/// Helper for tab methods that send a CDP command and resolve with void.
fn send_tab_void_cmd(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    method: &'static str,
    params: Option<Value>,
) -> JSResult {
    let this = cc.this();
    let payload = get_tab_payload(ctx, &this)?;
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

fn get_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<String> {
    let v = obj.get_property_str(ctx, key).ok()?;
    let s = v.as_string()?;
    Some(s.content().encode_utf8(false).to_string())
}

fn get_number_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<f64> {
    let v = obj.get_property_str(ctx, key).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_number(ctx).ok().map(|n| n.to_f64())
}

fn get_bool_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<bool> {
    let v = obj.get_property_str(ctx, key).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    Some(v.to_bool().unwrap_or(false))
}

/// Convert a `serde_json::Value` to a JS value.
fn js_from_json_value(ctx: &mut ScopedContext, val: &Value) -> JSResult {
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
