// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::path::PathBuf;

use crossbeam::channel::TryRecvError;
use serde_json::Value as JsonValue;
use zuqe::{Exception, JobState, ScopedContext};

/// Print a taken exception's message (+ traceback) instead of the opaque debug form.
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

/// Return `$AIKE_HOME` if set, otherwise the current working directory.
pub(crate) fn aike_home() -> PathBuf {
    std::env::var("AIKE_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Canonicalize a path, stripping the Windows `\\?\` prefix that `canonicalize()` adds.
///
/// On Windows, `std::fs::canonicalize()` returns paths with `\\?\` prefix (extended-length
/// path format, e.g. `\\?\C:\Users\...`). This breaks user-facing display and `starts_with`
/// comparisons. This wrapper strips that prefix for cross-platform consistency.
///
/// Also handles `\\?\UNC\server\share\...` 鈫?`\\server\share\...` conversion correctly.
pub(crate) fn canonicalize_path<P: AsRef<std::path::Path>>(
    path: P,
) -> std::io::Result<std::path::PathBuf> {
    let p = std::fs::canonicalize(path)?;

    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // UNC paths: `\\?\UNC\server\share\...` 鈫?`\\server\share\...`
            if let Some(unc) = rest
                .strip_prefix("UNC\\")
                .or_else(|| rest.strip_prefix("UNC/"))
            {
                return Ok(std::path::PathBuf::from(format!("\\{}", unc)));
            }
            // Regular paths: `\\?\C:\...` 鈫?`C:\...`
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

/// Start a persistent polling microtask driven by a crossbeam channel.
///
/// The poller calls `handler(msg, cx)` for each received message. If the
/// handler returns `true` the poller continues (`Again`); `false` triggers
/// a call to `on_finish()` and terminates (`Done`).
///
/// One job run handles EVERY message already queued, then checks
/// `has_entries(cx)`: live entries keep it polling (`Block`), otherwise
/// `on_finish(cx)` runs and it terminates. A disconnected channel always
/// triggers termination. Both `has_entries` and `on_finish` receive the
/// context so they can consult per-runtime state.
///
/// Draining per run is required, not an optimisation: `WakeSender` wakes the
/// loop through a single pending flag, so N messages sent while the loop was
/// running coalesce into ONE wakeup. Handling a single message per run would
/// then strand the other N-1 in the channel with no wake source left, and they
/// would sit there until unrelated work (a timer, another channel) happened to
/// tick the loop.
pub(crate) fn start_channel_poller<T, F>(
    ctx: &mut ScopedContext,
    label: &'static str,
    rx: zuqe::WakeReceiver<T>,
    handler: F,
    has_entries: impl Fn(&mut ScopedContext) -> bool + 'static,
    on_finish: impl Fn(&mut ScopedContext) + 'static,
) where
    T: 'static,
    F: Fn(T, &mut ScopedContext) -> bool + 'static,
{
    ctx.queue_job(
        move |cx| loop {
            match rx.try_recv() {
                Ok(msg) => {
                    let cont = handler(msg, cx);

                    // ES 搂9.5: handler returns bool, not Result 鈥?clear any
                    // exception it may have set via cx.throw_*().
                    if let exc @ Exception::Error(_) = cx.take_exception() {
                        log::warn!("[channel poller:{label}] handler left exception: {exc:?}");
                    }

                    if !cont {
                        on_finish(cx);
                        return Ok(JobState::Done);
                    }
                }
                Err(TryRecvError::Empty) => {
                    return Ok(if has_entries(cx) {
                        JobState::Block
                    } else {
                        on_finish(cx);
                        JobState::Done
                    });
                }
                Err(TryRecvError::Disconnected) => {
                    on_finish(cx);
                    return Ok(JobState::Done);
                }
            }
        },
        (),
    );
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
