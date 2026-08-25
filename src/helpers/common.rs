// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use async_openai::types::chat::CreateChatCompletionRequest;
use crossbeam::channel::TryRecvError;
use serde_json::Value as JsonValue;
use zuqe::{Exception, JobState, ScopedContext};

/// Print a taken exception's message (+ traceback) instead of the opaque
/// Debug form. Diagnostics only — never on a healthy path, never inlined.
#[cold]
#[cfg(debug_assertions)]
pub(crate) fn print_exc(ctx: &mut ScopedContext, exc: Exception) {
    if !matches!(exc, Exception::No) {
        let t = exc.to_text(ctx);
        match t.traceback {
            Some(tb) => eprintln!("[cleared-exc] {}\n{}", t.message, tb),
            None => eprintln!("[cleared-exc] {}", t.message),
        }
    }
}

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

fn now_utc() -> chrono::DateTime<chrono::Utc> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // epoch fallback keeps pre-1970 clock breakage non-fatal (as before)
    chrono::DateTime::from_timestamp(now.as_secs() as i64, 0)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
}

/// Format current UTC time as RFC 3339 (e.g. "2025-05-31T12:00:00Z")
pub(crate) fn format_timestamp_rfc3339() -> String {
    now_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Format current UTC time as compact string (e.g. "20250531_120000")
pub(crate) fn format_timestamp_compact() -> String {
    now_utc().format("%Y%m%d_%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_vectors() {
        let fmt = |t: i64| {
            chrono::DateTime::from_timestamp(t, 0)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };
        assert_eq!(fmt(0), "1970-01-01T00:00:00Z");
        assert_eq!(fmt(946684800), "2000-01-01T00:00:00Z");
        assert_eq!(fmt(951782400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(fmt(1600000000), "2020-09-13T12:26:40Z");

        let compact = |t: i64| {
            chrono::DateTime::from_timestamp(t, 0)
                .unwrap()
                .format("%Y%m%d_%H%M%S")
                .to_string()
        };
        assert_eq!(compact(0), "19700101_000000");
        assert_eq!(compact(951782400), "20000229_000000");
    }

    #[test]
    fn formatters_use_current_clock_shape() {
        let s = format_timestamp_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z') && s.as_bytes()[10] == b'T');
        let c = format_timestamp_compact();
        assert_eq!(c.len(), 15);
        assert!(c.as_bytes()[8] == b'_');
    }
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
