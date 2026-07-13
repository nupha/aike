// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use async_openai::types::chat::CreateChatCompletionRequest;
use crossbeam::channel::TryRecvError;
use serde_json::Value as JsonValue;
use zuqe::{Exception, JobState, ScopedContext};

/// Canonicalize a path, stripping the Windows `\\?\` prefix that `canonicalize()` adds.
///
/// On Windows, `std::fs::canonicalize()` returns paths with `\\?\` prefix (extended-length
/// path format, e.g. `\\?\C:\Users\...`). This breaks user-facing display and `starts_with`
/// comparisons. This wrapper strips that prefix for cross-platform consistency.
///
/// Also handles `\\?\UNC\server\share\...` → `\\server\share\...` conversion correctly.
pub(crate) fn canonicalize_path<P: AsRef<std::path::Path>>(
    path: P,
) -> std::io::Result<std::path::PathBuf> {
    let p = std::fs::canonicalize(path)?;

    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // UNC paths: `\\?\UNC\server\share\...` → `\\server\share\...`
            if let Some(unc) = rest
                .strip_prefix("UNC\\")
                .or_else(|| rest.strip_prefix("UNC/"))
            {
                return Ok(std::path::PathBuf::from(format!("\\{}", unc)));
            }
            // Regular paths: `\\?\C:\...` → `C:\...`
            return Ok(std::path::PathBuf::from(rest));
        }
    }

    Ok(p)
}

/// Convert a camelCase string to snake_case.
fn camel_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.char_indices() {
        if i > 0 && ch.is_uppercase() {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

/// Recursively convert all object keys in a JSON value from camelCase to snake_case.
pub(crate) fn json_keys_camel_to_snake(val: &mut JsonValue) {
    match val {
        JsonValue::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let snake = camel_to_snake(&key);
                if snake != key
                    && let Some(v) = map.remove(&key)
                {
                    map.insert(snake, v);
                }
            }
            // recurse into values
            for v in map.values_mut() {
                json_keys_camel_to_snake(v);
            }
        }
        JsonValue::Array(arr) => {
            for v in arr.iter_mut() {
                json_keys_camel_to_snake(v);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_chat_completion_req(
    mut json_val: serde_json::Value,
) -> Result<CreateChatCompletionRequest, serde_json::Error> {
    // Convert camelCase JS keys to snake_case for serde deserialization
    json_keys_camel_to_snake(&mut json_val);

    // Note: model is required, set default if missing
    if let Some(obj) = json_val.as_object_mut()
        && !obj.contains_key("model")
    {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String("".to_string()),
        );
    }

    serde_json::from_value(json_val)
}

pub(crate) fn now_unix_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format current UTC time as RFC 3339 (e.g. "2025-05-31T12:00:00Z")
pub(crate) fn format_timestamp_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let (ymd, hms) = secs_to_datetime(total_secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        ymd.0, ymd.1, ymd.2, hms.0, hms.1, hms.2
    )
}

/// Format current UTC time as compact string (e.g. "20250531_120000")
pub(crate) fn format_timestamp_compact() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = now.as_secs();
    let (ymd, hms) = secs_to_datetime(total_secs);
    format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        ymd.0, ymd.1, ymd.2, hms.0, hms.1, hms.2
    )
}

/// Convert seconds since epoch to (year, month, day) and (hours, minutes, seconds) in UTC.
fn secs_to_datetime(total_secs: u64) -> ((i64, u32, u32), (u32, u32, u32)) {
    let days = total_secs / 86400;
    let time_secs = total_secs % 86400;
    let hours = (time_secs / 3600) as u32;
    let minutes = ((time_secs % 3600) / 60) as u32;
    let seconds = (time_secs % 60) as u32;

    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let month_days = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0u32;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining < md {
            m = i as u32;
            break;
        }
        remaining -= md;
    }
    let day = (remaining + 1) as u32;
    ((y, m + 1, day), (hours, minutes, seconds))
}

/// Start a persistent polling microtask driven by a crossbeam channel.
///
/// The poller calls `handler(msg, cx)` for each received message. If the
/// handler returns `true` the poller continues (`Again`); `false` triggers
/// a call to `on_finish()` and terminates (`Done`).
///
/// When the channel is empty the poller checks `has_entries()`. If there
/// are still live entries it polls again; otherwise it calls `on_finish()`
/// and terminates.  A disconnected channel always triggers termination.
pub(crate) fn start_channel_poller<T, F>(
    ctx: &mut ScopedContext,
    label: &'static str,
    rx: crossbeam::channel::Receiver<T>,
    handler: F,
    has_entries: impl Fn() -> bool + 'static,
    on_finish: impl Fn() + 'static,
) where
    T: 'static,
    F: Fn(T, &mut ScopedContext) -> bool + 'static,
{
    ctx.queue_job(
        move |cx| match rx.try_recv() {
            Ok(msg) => {
                let cont = handler(msg, cx);

                // ES §9.5: handler returns bool, not Result — clear any
                // exception it may have set via cx.throw_*().
                if let exc @ Exception::Error(_) = cx.take_exception() {
                    log::warn!("[channel poller:{label}] handler left exception: {exc:?}");
                }

                if cont {
                    Ok(JobState::Again)
                } else {
                    on_finish();
                    Ok(JobState::Done)
                }
            }
            Err(TryRecvError::Empty) => {
                if has_entries() {
                    Ok(JobState::Again)
                } else {
                    on_finish();
                    Ok(JobState::Done)
                }
            }
            Err(TryRecvError::Disconnected) => {
                on_finish();
                Ok(JobState::Done)
            }
        },
        (),
    );
}
