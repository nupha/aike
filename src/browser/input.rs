// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Input domain methods.
//!
//! Mouse click, mouse move, keyboard press, text insertion.
//! These methods use native CDP Input domain calls.

use zuqe::{
    CallContext, JSResult, JSValue, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::browser::{
    cdp_input,
    class_page::{get_page_payload, send_page_void_cmd},
};

/// `page.mouseClick(x, y, options?)`
/// Clicks at pixel coordinates (x, y).
/// `options.button` — "left" (default), "middle", "right"
/// `options.clickCount` — 1 (default, single) or 2 (double)
pub(crate) fn js_page_mouse_click(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let x = cc.arg(0).to_number(ctx).map(|n| n.to_f64()).unwrap_or(0.0);
    let y = cc.arg(1).to_number(ctx).map(|n| n.to_f64()).unwrap_or(0.0);

    let mut button = "left".to_string();
    let mut click_count = 1i32;
    if cc.length() > 2
        && let Some(opts) = cc.arg(2).as_object()
    {
        if let Some(b) = crate::browser::class_page::get_str_prop(ctx, opts, "button") {
            button = b;
        }
        if let Some(c) = crate::browser::class_page::get_number_prop(ctx, opts, "clickCount") {
            click_count = c as i32;
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
        let result = cdp_input::mouse_click(&client, &session_id, x, y, &button, click_count).await;
        match result {
            Ok(()) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        }
    });

    Ok(promise.into())
}

/// `page.mouseMove(x, y)`
pub(crate) fn js_page_mouse_move(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let x = cc.arg(0).to_number(ctx).map(|n| n.to_f64()).unwrap_or(0.0);
    let y = cc.arg(1).to_number(ctx).map(|n| n.to_f64()).unwrap_or(0.0);

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
        let result = cdp_input::mouse_move(&client, &session_id, x, y).await;
        match result {
            Ok(()) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        }
    });

    Ok(promise.into())
}

/// `page.keyPress(key, options?)`
/// Simulates keyDown + char + keyUp for the given key.
/// `options.modifiers` — bitmask (1=Alt, 2=Ctrl, 4=Meta, 8=Shift)
pub(crate) fn js_page_key_press(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let key = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    let mut modifiers = 0i32;
    if cc.length() > 1
        && let Some(opts) = cc.arg(1).as_object()
    {
        if let Some(m) = crate::browser::class_page::get_number_prop(ctx, opts, "modifiers") {
            modifiers = m as i32;
        }
    }

    // Use the first character's code point as the virtual key code
    let code = key.chars().next().map(|c| c as i32).unwrap_or(0);

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
        // Sequence: keyDown → char → keyUp
        let r1 =
            cdp_input::dispatch_key_event(&client, &session_id, "keyDown", &key, code, modifiers)
                .await;
        if r1.is_err() {
            result_slot.commit(Err(r1.unwrap_err())).ok();
            return;
        }
        let r2 = cdp_input::dispatch_key_event(&client, &session_id, "char", &key, code, modifiers)
            .await;
        if r2.is_err() {
            result_slot.commit(Err(r2.unwrap_err())).ok();
            return;
        }
        let r3 =
            cdp_input::dispatch_key_event(&client, &session_id, "keyUp", &key, code, modifiers)
                .await;
        match r3 {
            Ok(()) => result_slot.commit(Ok(())).ok(),
            Err(e) => result_slot.commit(Err(e)).ok(),
        };
    });

    Ok(promise.into())
}

/// `page.insertText(text)`
/// Insert text via Input.insertText — handles CJK / Unicode reliably.
pub(crate) fn js_page_insert_text(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();

    send_page_void_cmd(
        ctx,
        &cc,
        "Input.insertText",
        Some(serde_json::json!({ "text": text })),
    )
}
