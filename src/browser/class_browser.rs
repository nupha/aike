// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Browser class — the top-level browser instance.
//!
//! Holds the CDP `CdpClient` (browser-level, no session) and the handle to the
//! spawned Chrome child process. All `Browser.*` methods are defined here.

use std::sync::Arc;

use zuqe::{
    Array, CallContext, ClassCtorDef, FieldDef, JS_PROP_C_W_E, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::browser::{
    cdp, chrome,
    class_page::{JSPage, PageNetworkState, PagePayload, get_number_prop, get_str_prop},
};

/// Payload attached to every `Browser` instance.
pub(crate) struct BrowserPayload {
    pub(crate) client: cdp::CdpClient,
    pub(crate) chrome_child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
}

impl GcTrace for BrowserPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

// ---------------------------------------------------------------------------
// Proto table — ALL Browser method entries
// ---------------------------------------------------------------------------

const BROWSER_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("newPage", 1, js_browser_new_page, 0),
    FieldDef::FUNC("close", 0, js_browser_close, 0),
    FieldDef::GETSET("isRunning", Some(js_browser_is_running), None, 0),
    // Browser domain — P0/P1
    FieldDef::FUNC("getVersion", 0, js_browser_get_version, 0),
    FieldDef::FUNC("getWindowBounds", 0, js_browser_get_window_bounds, 0),
    FieldDef::FUNC("setWindowBounds", 1, js_browser_set_window_bounds, 0),
    FieldDef::FUNC(
        "setDownloadBehavior",
        1,
        js_browser_set_download_behavior,
        0,
    ),
    // Target domain — P0/P1
    FieldDef::FUNC("listPages", 0, super::target::js_browser_list_pages, 0),
    FieldDef::FUNC("getTargets", 0, super::target::js_browser_get_targets, 0),
    FieldDef::FUNC("newContext", 0, super::target::js_browser_new_context, 0),
    FieldDef::FUNC(
        "disposeContext",
        1,
        super::target::js_browser_dispose_context,
        0,
    ),
    // Security domain — P0
    FieldDef::FUNC(
        "setIgnoreHTTPSErrors",
        1,
        super::security::js_browser_set_ignore_https_errors,
        0,
    ),
    FieldDef::FUNC(
        "handleCertificateError",
        1,
        super::security::js_browser_handle_certificate_error,
        0,
    ),
];

/// Data passed from async launch task to JS callback.
pub(crate) struct LaunchData {
    pub(crate) client: cdp::CdpClient,
    pub(crate) child: Option<tokio::process::Child>,
}

/// Normalized launch request shared by `launch` / `connect`.
struct LaunchRequest {
    headless: bool,
    port: u16,
    proxy_url: Option<String>,
    chrome_path: Option<String>,
    cdp_url: Option<String>,
}

impl Default for LaunchRequest {
    fn default() -> Self {
        Self {
            headless: true,
            port: 0, // 0 = auto-assign a free port
            proxy_url: None,
            chrome_path: None,
            cdp_url: None,
        }
    }
}

/// `launch(options?)` → Promise<Browser> (exported from the `browser` module)
pub(crate) fn js_browser_launch(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut req = LaunchRequest::default();

    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        req.proxy_url = get_str_prop(ctx, opts, "proxyUrl");
        req.chrome_path = get_str_prop(ctx, opts, "chromePath");
        req.cdp_url = get_str_prop(ctx, opts, "cdpUrl");
        if let Some(v) = get_number_prop(ctx, opts, "port") {
            req.port = v as u16;
        }
        let hv = opts
            .get_property_str(ctx, "headless")
            .unwrap_or(JSValue::UNDEFINED);
        if !hv.is_undefined() {
            req.headless = hv.to_bool().unwrap_or(false);
        }
    }

    browser_promise(ctx, req)
}

/// `Browser.connect(cdpUrl)` → Promise<Browser>
///
/// Attach to an already-running CDP server (e.g. from `Browser.spawn()` or an
/// external browser). The returned Browser does NOT own the process — `close()`
/// only disconnects.
pub(crate) fn js_browser_connect(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let url = cc
        .arg(0)
        .as_string(true)
        .map(|s| s.content().encode_utf8(false).to_string());
    let Some(cdp_url) = url else {
        return Err(ctx.throw_type_error("`connect` requires a `cdpUrl` string (ws://...)"));
    };

    browser_promise(
        ctx,
        LaunchRequest {
            cdp_url: Some(cdp_url),
            ..LaunchRequest::default()
        },
    )
}

/// Shared promise machinery for launch/connect: run the request, build the
/// Browser object when the CDP connection is ready.
fn browser_promise(ctx: &mut ScopedContext, req: LaunchRequest) -> JSResult {
    let LaunchRequest {
        headless,
        port,
        proxy_url,
        chrome_path,
        cdp_url,
    } = req;

    let result_slot = PromiseResultSlot::<Result<LaunchData, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let data = payload.map_err(|e| cx.throw_internal_error(&e))?;

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
        // With `cdpUrl` attach to an already-running CDP server (no child
        // process to own); otherwise spawn a browser and connect to it.
        let outcome = if let Some(url) = cdp_url {
            cdp::CdpClient::connect(&url).await.map(|(c, _)| (c, None))
        } else {
            match chrome::launch(headless, port, proxy_url.as_deref(), chrome_path.as_deref()).await
            {
                Ok((mut child, ws_url)) => match cdp::CdpClient::connect(&ws_url).await {
                    Ok((c, _)) => Ok((c, Some(child))),
                    Err(e) => {
                        let _ = child.start_kill();
                        Err(e)
                    }
                },
                Err(e) => Err(e),
            }
        };

        let (client, child) = match outcome {
            Ok(r) => r,
            Err(e) => {
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        let data = LaunchData { client, child };
        result_slot.commit(Ok(data)).ok();
    });

    Ok(promise.into())
}

/// Payload of a `BrowserProcess` handle returned by `Browser.spawn()`.
pub(crate) struct BrowserProcessPayload {
    /// Shared with the spawn registry so runtime-exit cleanup can kill the
    /// process even if this handle was GC-collected.
    pub(crate) child: ChildArcSlot,
    /// Resolved browser binary path (surfaced via the `bin` getter).
    pub(crate) bin: String,
    /// Ready browser-level CDP WebSocket URL (surfaced via `cdpUrl`).
    pub(crate) cdp_url: String,
}

impl GcTrace for BrowserProcessPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

/// `Browser.spawn(options?)` → Promise<BrowserProcess>
///
/// Spawn a browser CDP server without connecting. Returns a process handle
/// with the resolved `bin`, the ready `cdpUrl`, and an explicit `kill()`.
/// The handle does NOT own the process lifetime — dropping it is harmless;
/// the process dies on `kill()` or at runtime exit.
pub(crate) fn js_browser_spawn(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut bin: Option<String> = None;
    let mut args: Option<Vec<String>> = None;
    let mut headless = true;
    let mut port = 0u16; // 0 = auto-assign a free port
    let mut proxy_url: Option<String> = None;

    if cc.length() > 0
        && let Some(opts) = cc.arg(0).as_object()
    {
        bin = get_str_prop(ctx, opts, "bin");
        proxy_url = get_str_prop(ctx, opts, "proxyUrl");
        if let Some(v) = get_number_prop(ctx, opts, "port") {
            port = v as u16;
        }
        let hv = opts
            .get_property_str(ctx, "headless")
            .unwrap_or(JSValue::UNDEFINED);
        if !hv.is_undefined() {
            headless = hv.to_bool().unwrap_or(false);
        }
        if let Some(v) = opts
            .get_property_str(ctx, "args")
            .ok()
            .filter(|v| !v.is_undefined())
        {
            let arr = Array::try_from(v)?;
            let len = arr.get_length_prop(ctx)? as usize;
            let mut out = Vec::with_capacity(len);
            for i in 0..len {
                let s = arr
                    .get_property_u32(ctx, i as u32)?
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                out.push(s);
            }
            args = Some(out);
        }
    }

    if args.is_some() && bin.is_none() {
        return Err(ctx.throw_type_error("`spawn`: `args` requires `bin`"));
    }

    let result_slot = PromiseResultSlot::<Result<(String, String, ChildArcSlot), String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            let (bin, cdp_url, child) = payload.map_err(|e| cx.throw_internal_error(&e))?;

            let proc_obj = JSBrowserProcess::new_instance(
                cx,
                JSValue::UNDEFINED,
                BrowserProcessPayload {
                    child,
                    bin,
                    cdp_url,
                },
            )?;

            Ok(proc_obj.into())
        })?;

    tokio::spawn(async move {
        let r = chrome::spawn(
            bin.as_deref(),
            args.as_deref(),
            headless,
            port,
            proxy_url.as_deref(),
            false,
        )
        .await;

        let (bin, cdp_url, child) = match r {
            Ok(s) => (s.bin, s.cdp_url, s.child),
            Err(e) => {
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        // Register for runtime-exit cleanup, then hand the same Arc to the
        // JS handle.
        let handle: ChildArcSlot = Arc::new(tokio::sync::Mutex::new(Some(child)));
        chrome::register_spawned_child(&handle);

        result_slot.commit(Ok((bin, cdp_url, handle))).ok();
    });

    Ok(promise.into())
}

type ChildArcSlot = Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>;

/// `process.kill()` — kill the spawned browser process; resolves after exit.
fn js_process_kill(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = JSBrowserProcess::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a BrowserProcess instance"))?;
    let child = payload.child.clone();

    let result_slot = PromiseResultSlot::<Result<(), String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), |cx, payload| -> JSResult {
            payload.map_err(|e| cx.throw_internal_error(&e))?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let mut guard = child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        result_slot.commit(Ok(())).ok();
    });

    Ok(promise.into())
}

// magic constants for the BrowserProcess getters
const GETTER_BIN: i16 = 0;
const GETTER_CDP_URL: i16 = 1;
const GETTER_PID: i16 = 2;

/// Shared getter for `bin` / `cdpUrl` / `pid`.
/// Selected by `magic`: 0=bin, 1=cdpUrl, 2=pid.
fn js_process_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = JSBrowserProcess::payload_of(&this)
        .ok_or_else(|| ctx.throw_type_error("not a BrowserProcess instance"))?;
    match cc.magic() as i16 {
        GETTER_BIN => {
            let s = ctx.create_string_from_utf8(payload.bin.clone().into_bytes())?;
            Ok(JSValue::make_string(s))
        }
        GETTER_CDP_URL => {
            let s = ctx.create_string_from_utf8(payload.cdp_url.clone().into_bytes())?;
            Ok(JSValue::make_string(s))
        }
        GETTER_PID => {
            let pid = payload
                .child
                .try_lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|c| c.id()));
            match pid {
                Some(pid) => Ok(JSValue::from(pid as i64)),
                None => Ok(JSValue::NULL),
            }
        }
        _ => Err(ctx.throw_internal_error("invalid BrowserProcess getter")),
    }
}

const PROCESS_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("kill", 0, js_process_kill, 0),
    FieldDef::GETSET("bin", Some(js_process_get), None, GETTER_BIN),
    FieldDef::GETSET("cdpUrl", Some(js_process_get), None, GETTER_CDP_URL),
    FieldDef::GETSET("pid", Some(js_process_get), None, GETTER_PID),
];

const PROCESS_CTOR: ClassCtorDef = ClassCtorDef::new(
    |ctx: &mut ScopedContext, _cc: CallContext| {
        Err(ctx
            .throw_type_error("BrowserProcess cannot be constructed directly; use Browser.spawn()"))
    },
    0,
    false,
);

def_native_object!(
    "BrowserProcess",
    JSBrowserProcess,
    BrowserProcessPayload,
    PROCESS_PROTO,
    Some(PROCESS_CTOR)
);

/// Extract the `BrowserPayload` from a `Browser` JS value.
pub(crate) fn get_browser_payload<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a BrowserPayload, i32> {
    JSBrowser::payload_of(val).ok_or_else(|| ctx.throw_type_error("not a Browser instance"))
}

/// Shared helper: void-returning CDP command sent at the Browser level
/// (no session id). Mirrors `send_page_void_cmd` for browser-scope commands.
pub(crate) fn send_browser_void_cmd(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    method: &'static str,
    params: Option<serde_json::Value>,
) -> JSResult {
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
        let rx = client.send(method, params, None);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        match result {
            Ok(_) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

/// `browser.newPage(url?, contextId?)` → Promise<Page>
/// `url` may be a string or an options object `{ url, contextId }`.
fn js_browser_new_page(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();
    let client_cb = client.clone();

    // Resolve url + optional browser context id from the argument.
    let mut url = "about:blank".to_string();
    let mut context_id: Option<String> = None;
    if cc.length() > 0 {
        if let Some(s) = cc.arg(0).as_string(true) {
            url = s.content().encode_utf8(false).to_string();
        } else if let Some(obj) = cc.arg(0).as_object() {
            if let Some(u) = get_str_prop(ctx, obj, "url") {
                url = u;
            }
            context_id = get_str_prop(ctx, obj, "contextId");
        }
    }

    let result_slot = PromiseResultSlot::<Result<(String, String), String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, payload| -> JSResult {
            let (target_id, session_id) = payload.map_err(|e| cx.throw_internal_error(&e))?;

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
            page.define_property_value_str(
                cx,
                "targetId",
                JSValue::make_string(js_id),
                JS_PROP_C_W_E,
            )?;
            Ok(page.into())
        },
    )?;

    tokio::spawn(async move {
        // Build Target.createTarget params (optionally scoped to a context).
        let mut params = serde_json::json!({ "url": url });
        if let Some(cid) = context_id {
            params["browserContextId"] = serde_json::Value::String(cid);
        }
        let rx = client.send("Target.createTarget", Some(params), None);
        let resp = match rx.await {
            Ok(r) => r,
            Err(_) => {
                result_slot.commit(Err("channel closed".into())).ok();
                return;
            }
        };
        let result = resp.and_then(|v| {
            let target_id = v
                .get("targetId")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "no targetId".to_string())?
                .to_string();
            Ok(target_id)
        });
        let target_id = match result {
            Ok(t) => t,
            Err(e) => {
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        let attach_rx = client.send(
            "Target.attachToTarget",
            Some(serde_json::json!({ "targetId": target_id, "flatten": true })),
            None,
        );
        let attach_resp = match attach_rx.await {
            Ok(r) => r,
            Err(_) => {
                result_slot.commit(Err("channel closed".into())).ok();
                return;
            }
        };
        let session_id = match attach_resp.and_then(|v| {
            v.get("sessionId")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "no sessionId".to_string())
                .map(|s| s.to_string())
        }) {
            Ok(s) => s,
            Err(e) => {
                result_slot.commit(Err(e)).ok();
                return;
            }
        };

        result_slot.commit(Ok((target_id, session_id))).ok();
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
            payload.map_err(|e| cx.throw_internal_error(&e))?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        // Attach mode (no child process): don't send `Browser.close` — the
        // external browser is not ours; just drop the connection.
        let owns_process = chrome_child
            .try_lock()
            .map(|g| g.is_some())
            .unwrap_or(false);
        if owns_process {
            let rx = client.send("Browser.close", None, None);
            let _ = rx.await;
        }
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

/// `browser.getVersion()` — `Browser.getVersion`.
/// Returns `{ protocolVersion, product, revision, userAgent, jsVersion }`.
fn js_browser_get_version(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
        let rx = client.send("Browser.getVersion", None, None);
        let result = rx.await.unwrap_or(Err("channel closed".into()));
        result_slot.commit(result).ok();
    });

    Ok(promise.into())
}

/// Resolve the browser's main window id via `Browser.getWindowForTarget`.
async fn browser_window_id(client: &cdp::CdpClient) -> Result<String, String> {
    let rx = client.send(
        "Browser.getWindowForTarget",
        Some(serde_json::json!({})),
        None,
    );
    let resp: serde_json::Value = rx.await.unwrap_or(Err("channel closed".into()))?;
    resp.get("windowId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "no windowId".to_string())
}

/// `browser.getWindowBounds()` — `Browser.getWindowBounds`.
fn js_browser_get_window_bounds(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
        let wid = browser_window_id(&client).await;
        let r: Result<serde_json::Value, String> = match wid {
            Ok(id) => {
                let rx = client.send(
                    "Browser.getWindowBounds",
                    Some(serde_json::json!({ "windowId": id })),
                    None,
                );
                rx.await.unwrap_or(Err("channel closed".into()))
            }
            Err(e) => Err(e),
        };
        result_slot.commit(r).ok();
    });

    Ok(promise.into())
}

/// `browser.setWindowBounds(bounds)` — `Browser.setWindowBounds`.
/// `bounds` is `{ left?, top?, width?, height?, windowState? }`.
fn js_browser_set_window_bounds(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut bounds = serde_json::Map::new();
    if cc.length() > 0
        && let Some(obj) = cc.arg(0).as_object()
    {
        if let Some(v) = get_number_prop(ctx, obj, "left") {
            bounds.insert("left".into(), serde_json::Value::from(v as i32));
        }
        if let Some(v) = get_number_prop(ctx, obj, "top") {
            bounds.insert("top".into(), serde_json::Value::from(v as i32));
        }
        if let Some(v) = get_number_prop(ctx, obj, "width") {
            bounds.insert("width".into(), serde_json::Value::from(v as i32));
        }
        if let Some(v) = get_number_prop(ctx, obj, "height") {
            bounds.insert("height".into(), serde_json::Value::from(v as i32));
        }
        if let Some(s) = get_str_prop(ctx, obj, "windowState") {
            bounds.insert("windowState".into(), serde_json::Value::String(s));
        }
    }

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
        let wid = browser_window_id(&client).await;
        let r: Result<(), String> = match wid {
            Ok(id) => {
                let rx = client.send(
                    "Browser.setWindowBounds",
                    Some(serde_json::json!({ "windowId": id, "bounds": bounds })),
                    None,
                );
                match rx.await {
                    Ok(_) => Ok(()),
                    Err(_) => Err("channel closed".to_string()),
                }
            }
            Err(e) => Err(e),
        };
        result_slot.commit(r).ok();
    });

    Ok(promise.into())
}

/// `browser.setDownloadBehavior(path)` — global `Browser.setDownloadBehavior`.
/// Empty path → "deny", otherwise "allow" with the given download path.
fn js_browser_set_download_behavior(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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
    send_browser_void_cmd(ctx, &cc, "Browser.setDownloadBehavior", Some(params))
}

/// `browser.isRunning` getter
fn js_browser_is_running(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let guard = payload.chrome_child.try_lock();
    match guard {
        Ok(inner) => Ok(JSValue::from(inner.is_some())),
        Err(_) => Ok(JSValue::FALSE),
    }
}

const BROWSER_CTOR: ClassCtorDef = ClassCtorDef::new(
    |ctx: &mut ScopedContext, _cc: CallContext| {
        Err(ctx.throw_type_error(
            "Browser cannot be constructed directly; use Browser.launch() to start a browser",
        ))
    },
    0,
    false,
)
.entries(&[
    FieldDef::FUNC("launch", 1, js_browser_launch, 0),
    FieldDef::FUNC("spawn", 1, js_browser_spawn, 0),
    FieldDef::FUNC("connect", 1, js_browser_connect, 0),
]);

def_native_object!(
    "Browser",
    JSBrowser,
    BrowserPayload,
    BROWSER_PROTO,
    Some(BROWSER_CTOR)
);
