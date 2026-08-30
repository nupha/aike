// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Fetch domain (request interception, flat on `Page`).
//!
//! `setRequestInterception(bool)` / `continueRequest(reqId, opts?)` /
//! `abortRequest(reqId, reason?)` / `respondRequest(reqId, opts?)` /
//! `onRequestPaused(callback)`. Underlying CDP: `Fetch.*`.

use std::cell::RefCell;

use base64::Engine as _;
use crossbeam::channel;
use zuqe::{CallContext, JS_PROP_C_W_E, JSResult, JSValue, ScopedContext};

use crate::browser::{
    cdp_network,
    class_page::{get_number_prop, get_page_payload, get_str_prop, send_page_void_cmd},
};

// ---------------------------------------------------------------------------
// Per-session interception callback
// ---------------------------------------------------------------------------

thread_local! {
    // session_id -> registered `onRequestPaused` callback (last wins).
    pub(crate) static FETCH_CALLBACK: RefCell<std::collections::HashMap<String, JSValue>> =
        RefCell::new(std::collections::HashMap::new());
}

/// `page.setRequestInterception(enabled)` — enable/disable Fetch interception.
pub(crate) fn js_page_set_request_interception(
    ctx: &mut ScopedContext,
    cc: CallContext,
) -> JSResult {
    let enabled = cc.arg(0).to_bool().unwrap_or(false);
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let client = payload.client.clone();
    let session_id = payload.session_id.clone();

    if enabled {
        let (tx, rx) = channel::unbounded::<serde_json::Value>();
        let client_ev = client.clone();
        let session_ev = session_id.clone();
        let wake = ctx.wake_handle();
        tokio::spawn(async move {
            let _ = client_ev
                .send(
                    "Fetch.enable",
                    Some(serde_json::json!({ "patterns": [{ "urlPattern": "*" }] })),
                    Some(&session_ev),
                )
                .await;
            // Enabling the Network domain up-front ensures request bodies are
            // tracked, so `Network.getRequestPostData` can read them later.
            let _ = cdp_network::enable_network(&client_ev, &session_ev).await;
            let mut event_rx = client_ev.subscribe_events();
            loop {
                match event_rx.recv().await {
                    Ok(evt)
                        if evt.method == "Fetch.requestPaused"
                            && evt.session_id.as_deref() == Some(&session_ev) =>
                    {
                        let msg = serde_json::json!({
                            "sessionId": evt.session_id,
                            "params": evt.params,
                        });
                        if tx.send(msg).is_err() {
                            break;
                        }
                        wake.wake();
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        crate::helpers::start_channel_poller(
            ctx,
            "fetch",
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
                    FETCH_CALLBACK.with(|m| sess.as_ref().and_then(|s| m.borrow().get(s).cloned()));

                if let (Some(cb), Some(_sess)) = (cb, sess) {
                    let mut obj = cx.create_base_object().ok();
                    if let Some(o) = obj.as_mut() {
                        if let Some(rid) = params.get("requestId").and_then(|v| v.as_str()) {
                            if let Ok(s) = cx.create_string_from_utf8(rid.as_bytes().to_vec()) {
                                let _ = o.define_property_value_str(
                                    cx,
                                    "requestId",
                                    JSValue::make_string(s),
                                    JS_PROP_C_W_E,
                                );
                            }
                        }
                        if let Some(req) = params.get("request") {
                            let req_val =
                                JSValue::from_serde_json(cx, req).unwrap_or(JSValue::UNDEFINED);
                            let _ =
                                o.define_property_value_str(cx, "request", req_val, JS_PROP_C_W_E);
                        }
                        for key in ["frameId", "resourceType", "networkId"] {
                            if let Some(v) = params.get(key).and_then(|x| x.as_str()) {
                                if let Ok(s) = cx.create_string_from_utf8(v.as_bytes().to_vec()) {
                                    let _ = o.define_property_value_str(
                                        cx,
                                        key,
                                        JSValue::make_string(s),
                                        JS_PROP_C_W_E,
                                    );
                                }
                            }
                        }
                    }

                    // Poller returns bool (can't `?`); a throwing event callback
                    // is taken here so we can keep returning `true` (keep polling)
                    // without a lingering exception.
                    if let Some(o) = obj
                        && cb.call(cx, JSValue::UNDEFINED, &[o.into()]).is_err()
                    {
                        let exc = cx.take_exception();
                        #[cfg(debug_assertions)]
                        crate::helpers::print_exc(cx, exc);
                    }
                }

                true
            },
            || true, // interception stays active until disabled
            || {},
        );

        // Resolve once Fetch.enable has been issued.
        send_page_void_cmd(
            ctx,
            &cc,
            "Fetch.enable",
            Some(serde_json::json!({ "patterns": [{ "urlPattern": "*" }] })),
        )
    } else {
        FETCH_CALLBACK.with(|m| m.borrow_mut().remove(&session_id));
        send_page_void_cmd(ctx, &cc, "Fetch.disable", None)
    }
}

/// `page.continueRequest(reqId, opts?)` — `Fetch.continueRequest`.
pub(crate) fn js_page_continue_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let req_id = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let _payload = get_page_payload(ctx, &this)?;

    let mut params = serde_json::json!({ "requestId": req_id });
    if cc.length() > 1
        && let Some(obj) = cc.arg(1).as_object()
    {
        if let Some(u) = get_str_prop(ctx, obj, "url") {
            params["url"] = serde_json::Value::String(u);
        }
        if let Some(m) = get_str_prop(ctx, obj, "method") {
            params["method"] = serde_json::Value::String(m);
        }
        if let Some(h) = obj
            .get_property_str(ctx, "headers")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.as_object())
        {
            params["headers"] = h.to_serde_json(ctx).unwrap_or_default();
        }
        if let Some(p) = get_str_prop(ctx, obj, "postData") {
            params["postData"] = serde_json::Value::String(p);
        }
    }

    send_page_void_cmd(ctx, &cc, "Fetch.continueRequest", Some(params))
}

/// `page.abortRequest(reqId, reason?)` — `Fetch.failRequest`.
pub(crate) fn js_page_abort_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let req_id = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    let reason = if cc.length() > 1 {
        if let Some(obj) = cc.arg(1).as_object() {
            get_str_prop(ctx, obj, "reason").unwrap_or_else(|| "Failed".to_string())
        } else {
            cc.arg(1)
                .to_string(ctx)
                .map(|s| s.content().encode_utf8(false).to_string())
                .unwrap_or_else(|_| "Failed".to_string())
        }
    } else {
        "Failed".to_string()
    };

    let this = cc.this();
    let _payload = get_page_payload(ctx, &this)?;

    send_page_void_cmd(
        ctx,
        &cc,
        "Fetch.failRequest",
        Some(serde_json::json!({ "requestId": req_id, "errorReason": reason })),
    )
}

/// `page.respondRequest(reqId, opts?)` — `Fetch.fulfillRequest`.
pub(crate) fn js_page_respond_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let req_id = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let this = cc.this();
    let _ = get_page_payload(ctx, &this)?;

    let mut params = serde_json::json!({ "requestId": req_id });
    if cc.length() > 1
        && let Some(obj) = cc.arg(1).as_object()
    {
        if let Some(code) = get_number_prop(ctx, obj, "status") {
            params["responseCode"] = serde_json::json!(code as i32);
        }
        if let Some(h) = obj
            .get_property_str(ctx, "headers")
            .ok()
            .filter(|v| !v.is_undefined())
            .and_then(|v| v.as_object())
        {
            params["responseHeaders"] = h.to_serde_json(ctx).unwrap_or_default();
        }
        if let Some(body) = get_str_prop(ctx, obj, "body") {
            let b64 = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
            params["body"] = serde_json::Value::String(b64);
        }
        if let Some(ct) = get_str_prop(ctx, obj, "contentType") {
            if params.get("responseHeaders").is_none() {
                params["responseHeaders"] =
                    serde_json::json!([{ "name": "Content-Type", "value": ct }]);
            }
        }
    }

    send_page_void_cmd(ctx, &cc, "Fetch.fulfillRequest", Some(params))
}

/// `page.onRequestPaused(callback)` — register the interception callback.
pub(crate) fn js_page_on_request_paused(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = get_page_payload(ctx, &this)?;
    let session_id = payload.session_id.clone();
    let callback = cc.arg(0).clone();
    FETCH_CALLBACK.with(|m| {
        m.borrow_mut().insert(session_id, callback);
    });
    Ok(JSValue::UNDEFINED)
}
