// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Browser class — Security domain methods (flat on `Browser`).
//!
//! `setIgnoreHTTPSErrors` (P0) and `handleCertificateError` (P0, callback).
//! Underlying CDP: `Security.*`.

use zuqe::{CallContext, Callback, JS_PROP_C_W_E, JSResult, JSValue, ScopedContext};

use crate::{
    aike::AikeRuntimeState,
    browser::class_browser::{get_browser_payload, send_browser_void_cmd},
};

const CERT_ACCEPT_MAGIC: i16 = 1;

/// `js_cert_accept` — the `accept(bool)` method attached to the event object.
/// Resolves the in-flight cert error via `Security.handleCertificateError`.
fn js_cert_accept(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let accept = cc.arg(0).to_bool().unwrap_or(false);
    let (event_id, client) = {
        let st = AikeRuntimeState::of(ctx);
        (st.browser.cert_event_id, st.browser.cert_client.clone())
    };
    if let (Some(id), Some(client)) = (event_id, client) {
        let action = if accept { "accept" } else { "cancel" };
        client.send(
            "Security.handleCertificateError",
            Some(serde_json::json!({ "eventId": id, "action": action })),
            None,
        );
    }

    Ok(JSValue::UNDEFINED)
}

/// `browser.setIgnoreHTTPSErrors(bool)` — `Security.setIgnoreCertificateErrors`.
pub(crate) fn js_browser_set_ignore_https_errors(
    ctx: &mut ScopedContext,
    cc: CallContext,
) -> JSResult {
    let enabled = cc.arg(0).to_bool().unwrap_or(false);
    send_browser_void_cmd(
        ctx,
        &cc,
        "Security.setIgnoreCertificateErrors",
        Some(serde_json::json!({ "ignore": enabled })),
    )
}

/// `browser.handleCertificateError(callback)` — register a listener invoked for
/// each `Security.certificateError`. The callback receives an event object
/// `{ eventId, errorType, requestURL, accept(bool) }`; call `accept(true)` to
/// trust the certificate, `accept(false)` to cancel.
pub(crate) fn js_browser_handle_certificate_error(
    ctx: &mut ScopedContext,
    cc: CallContext,
) -> JSResult {
    let this = cc.this();
    let payload = get_browser_payload(ctx, &this)?;
    let client = payload.client.clone();

    let callback = cc.arg(0).clone();
    {
        let st = AikeRuntimeState::of(ctx);
        st.browser.cert_callback = Some(callback);
        st.browser.cert_client = Some(client.clone());
    }

    // Enable Security domain (idempotent) so certificateError events arrive.
    let _ = send_browser_void_cmd(ctx, &cc, "Security.enable", None);

    let (tx, rx) = ctx.make_local_channel::<serde_json::Value>();
    tokio::spawn(async move {
        let mut event_rx = client.subscribe_events();
        loop {
            match event_rx.recv().await {
                Ok(evt) if evt.method == "Security.certificateError" => {
                    if tx.send(evt.params.clone()).is_err() {
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
        "certificateError",
        rx,
        |params, cx| {
            let cb = AikeRuntimeState::of(cx).browser.cert_callback;
            if let Some(cb) = cb {
                let event_id = params.get("eventId").and_then(|v| v.as_i64()).unwrap_or(0);
                AikeRuntimeState::of(cx).browser.cert_event_id = Some(event_id);

                let mut obj = cx.create_base_object().ok();
                if let Some(o) = obj.as_mut() {
                    let _ = o.define_property_value_str(
                        cx,
                        "eventId",
                        JSValue::from(event_id as i32),
                        JS_PROP_C_W_E,
                    );
                    if let Some(et) = params.get("errorType").and_then(|v| v.as_str())
                        && let Ok(s) = cx.create_string_from_utf8(et.as_bytes().to_vec())
                    {
                        let _ = o.define_property_value_str(
                            cx,
                            "errorType",
                            JSValue::make_string(s),
                            JS_PROP_C_W_E,
                        );
                    }

                    if let Some(u) = params.get("requestURL").and_then(|v| v.as_str())
                        && let Ok(s) = cx.create_string_from_utf8(u.as_bytes().to_vec())
                    {
                        let _ = o.define_property_value_str(
                            cx,
                            "requestURL",
                            JSValue::make_string(s),
                            JS_PROP_C_W_E,
                        );
                    }

                    // Attach the accept(bool) method.
                    let accept_fn = cx
                        .create_native_function(
                            Callback::Func(js_cert_accept),
                            "accept",
                            1,
                            CERT_ACCEPT_MAGIC,
                        )
                        .ok();

                    if let Some(f) = accept_fn {
                        let _ = o.define_property_value_str(cx, "accept", f.into(), JS_PROP_C_W_E);
                    }
                }

                if let Some(o) = obj
                    && cb.call(cx, JSValue::UNDEFINED, &[o.into()]).is_err()
                {
                    let exc = cx.take_exception();
                    #[cfg(debug_assertions)]
                    crate::helpers::print_exc(cx, exc);
                }
                AikeRuntimeState::of(cx).browser.cert_event_id = None;
            }

            true
        },
        |cx| AikeRuntimeState::of(cx).browser.cert_callback.is_some(),
        |cx| {
            let st = AikeRuntimeState::of(cx);
            st.browser.cert_callback = None;
            st.browser.cert_client = None;
            st.browser.cert_event_id = None;
        },
    );

    Ok(JSValue::UNDEFINED)
}
