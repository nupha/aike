// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Page class — Emulation domain methods (flat on `Page`).
//!
//! `setGeolocation` / `setMediaFeatures` / `setTimezone` / `setLocale` /
//! `setTouchEnabled` / `setCPUThrottling`. Underlying CDP: `Emulation.*`.
//!
//! All are void-returning commands — built on `send_page_void_cmd`.

use zuqe::{CallContext, JSResult, ScopedContext};

use crate::browser::class_page::{get_str_prop, send_page_void_cmd};

/// `page.setGeolocation(latitude, longitude, accuracy?)`
pub(crate) fn js_page_set_geolocation(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let latitude = cc.arg(0).to_number(ctx).map(|n| n.to_f64()).unwrap_or(0.0);
    let longitude = cc.arg(1).to_number(ctx).map(|n| n.to_f64()).unwrap_or(0.0);
    let accuracy = if cc.length() > 2 {
        cc.arg(2).to_number(ctx).map(|n| n.to_f64()).unwrap_or(1.0)
    } else {
        1.0
    };
    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setGeolocationOverride",
        Some(serde_json::json!({
            "latitude": latitude,
            "longitude": longitude,
            "accuracy": accuracy,
        })),
    )
}

/// `page.setMediaFeatures(features)` — `features` is an array of `{ name, value }`.
pub(crate) fn js_page_set_media_features(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut features = Vec::new();
    if cc.length() > 0
        && let Some(arr) = cc.arg(0).as_object_Array()
    {
        for item in arr.payload().values() {
            if let Some(obj) = item.as_object() {
                let name = get_str_prop(ctx, &obj, "name").unwrap_or_default();
                let value = get_str_prop(ctx, &obj, "value").unwrap_or_default();
                features.push(serde_json::json!({ "name": name, "value": value }));
            }
        }
    }
    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setEmulatedMedia",
        Some(serde_json::json!({ "features": features })),
    )
}

/// `page.setTimezone(timezone)` — e.g. "America/New_York".
pub(crate) fn js_page_set_timezone(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let tz = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setTimezoneOverride",
        Some(serde_json::json!({ "timezoneId": tz })),
    )
}

/// `page.setLocale(locale)` — e.g. "en-US".
pub(crate) fn js_page_set_locale(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let locale = cc
        .arg(0)
        .to_string(ctx)
        .map(|s| s.content().encode_utf8(false).to_string())
        .unwrap_or_default();
    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setLocaleOverride",
        Some(serde_json::json!({ "locale": locale })),
    )
}

/// `page.setTouchEnabled(bool)`
pub(crate) fn js_page_set_touch_enabled(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let enabled = cc.arg(0).to_bool().unwrap_or(false);
    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setTouchEmulationEnabled",
        Some(serde_json::json!({ "enabled": enabled })),
    )
}

/// `page.setCPUThrottling(rate)` — CPU throttling rate (1 = no throttle).
pub(crate) fn js_page_set_cpu_throttling(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let rate = if cc.length() > 0 {
        cc.arg(0).to_number(ctx).map(|n| n.to_f64()).unwrap_or(1.0)
    } else {
        1.0
    };
    send_page_void_cmd(
        ctx,
        &cc,
        "Emulation.setCPUThrottlingRate",
        Some(serde_json::json!({ "rate": rate })),
    )
}
