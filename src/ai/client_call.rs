// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Per-call control layer shared by every client class.
//!
//! Everything here steers HOW a request is issued (cancellation, timeouts,
//! coalescing, headers) and never what is submitted: none of it is serialized
//! into the API body. `DecisionClient`, `EmbeddingClient` and the Responses
//! endpoint import these helpers, so they live apart from the OpenAI
//! chat-completions specifics in `class_client.rs`.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;
use zuqe::{CallContext, IntOrFloat, JSValue, Object, ScopedContext};

use crate::whatwg::abort::JSAbortSignal;

/// Error surfaced to JS when a request is cancelled via the `signal` call opt.
pub(super) const ABORT_MSG: &str = "request aborted by AbortSignal";

/// Cancellation shared by a streaming iterator's JS side and its reading task.
///
/// `cancel()` runs on the JS thread when the consumer abandons iteration early
/// (`for await ... break`, iterator `.return()`); the reading task's outer
/// `tokio::select!` then drops the request in flight, closing the upstream
/// connection so generation stops server-side too.
pub(super) struct StreamCancel {
    cancelled: AtomicBool,
    notify: Notify,
}

impl StreamCancel {
    pub(super) fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    pub(super) fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.notify.notified()
    }
}

impl Default for StreamCancel {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-call control options — the second argument of
/// `chat`/`chatOnce`/`responses`/`responsesOnce`. Never serialized into the
/// request body; only steers cancellation and timeouts.
pub(super) struct CallOpts {
    pub(super) signal: JSValue,
    pub(super) timeout: Option<u64>,
    pub(super) stallTimeout: Option<u64>,
    /// Merge consecutive pure-content chunks arriving within this window into
    /// one chunk, amortizing the per-chunk commit/wake/JS-convert cost. Window
    /// in milliseconds; 0 (or negative) disables coalescing. Changes the
    /// yielded chunk granularity, so it defaults to off.
    pub(super) coalesce_ms: u64,
}

/// Read an optional positive-integer seconds control option.
/// TypeError for non-numbers, RangeError for non-positive or fractional values.
pub(super) fn opt_secs_prop(
    ctx: &mut ScopedContext,
    obj: &Object,
    key: &str,
) -> Result<Option<u64>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`{key}` must be a number")));
    }
    let n = v.to_number(ctx)?;
    let ok = matches!(n, IntOrFloat::Int64(i) if i > 0)
        || matches!(n, IntOrFloat::Float64(d) if d > 0.0 && d.fract() == 0.0);
    if !ok {
        return Err(ctx.throw_range_error(&format!("`{key}` must be a positive integer")));
    }
    Ok(Some(match n {
        IntOrFloat::Int64(i) => i as u64,
        IntOrFloat::Float64(d) => d as u64,
    }))
}

/// Resolve the control-opts second argument to an object; `None` when omitted.
/// `undefined` → defaults; anything but an object (including `null`) → TypeError.
pub(super) fn opts_obj(ctx: &mut ScopedContext, cc: &CallContext) -> Result<Option<Object>, i32> {
    let v = cc.arg(1);
    if v.is_undefined() {
        return Ok(None);
    }
    Ok(Some(v.as_object().ok_or_else(|| {
        ctx.throw_type_error("opts must be an object")
    })?))
}

/// Parse the control-only options every client call shares: cancellation and
/// per-request timeout. Streaming knobs (`stallTimeout`, `coalesce`) are added
/// on top by `parse_call_opts`.
pub(super) fn parse_basic_call_opts(
    ctx: &mut ScopedContext,
    cc: &CallContext,
) -> Result<CallOpts, i32> {
    let Some(obj) = opts_obj(ctx, cc)? else {
        return Ok(CallOpts {
            signal: JSValue::UNDEFINED,
            timeout: None,
            stallTimeout: None,
            coalesce_ms: 0,
        });
    };

    let signal = obj.get_property_str(ctx, "signal")?;
    if !signal.is_undefined_or_null() && JSAbortSignal::payload_of(&signal).is_none() {
        return Err(ctx.throw_type_error("`signal` must be an AbortSignal"));
    }

    Ok(CallOpts {
        signal,
        timeout: opt_secs_prop(ctx, &obj, "timeout")?,
        stallTimeout: None,
        coalesce_ms: 0,
    })
}

/// Parse the optional control-opts second argument. `undefined` → all defaults;
/// anything but an object (including `null`) → TypeError.
pub(super) fn parse_call_opts(ctx: &mut ScopedContext, cc: &CallContext) -> Result<CallOpts, i32> {
    let mut opts = parse_basic_call_opts(ctx, cc)?;
    let Some(obj) = opts_obj(ctx, cc)? else {
        return Ok(opts);
    };

    // Coalesce window in ms: any integer; <= 0 disables coalescing.
    let coalesce_val = obj.get_property_str(ctx, "coalesce")?;
    opts.coalesce_ms = if coalesce_val.is_undefined() {
        0
    } else if !coalesce_val.is_number() {
        return Err(ctx.throw_type_error("`coalesce` must be a number"));
    } else {
        let n = coalesce_val.to_number(ctx)?;
        let ok = matches!(n, IntOrFloat::Int64(_))
            || matches!(n, IntOrFloat::Float64(d) if d.is_finite() && d.fract() == 0.0);
        if !ok {
            return Err(ctx.throw_range_error("`coalesce` must be an integer"));
        }
        match n {
            IntOrFloat::Int64(i) => i.max(0) as u64,
            IntOrFloat::Float64(d) => d.max(0.0) as u64,
        }
    };

    opts.stallTimeout = opt_secs_prop(ctx, &obj, "stallTimeout")?;
    Ok(opts)
}

/// Read an optional string constructor option. `undefined` → keep the current
/// value; any non-string (including `null`) → TypeError.
pub(super) fn opt_str_prop(
    ctx: &mut ScopedContext,
    obj: &Object,
    key: &str,
) -> Result<Option<String>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    match v.as_string(true) {
        Some(s) => Ok(Some(s.content().encode_utf8(false).to_string())),
        None => Err(ctx.throw_type_error(&format!("`{key}` must be a string"))),
    }
}

/// Read an optional `{headers}` constructor option into a string map. Non-string
/// keys/values are skipped (a header value must be a string to be sendable).
pub(super) fn parse_headers_prop(
    ctx: &mut ScopedContext,
    obj: &Object,
) -> Result<std::collections::HashMap<String, String>, i32> {
    let mut headers = std::collections::HashMap::new();
    let headers_val = obj.get_property_str(ctx, "headers")?;
    if let Some(mut o) = headers_val.as_object() {
        for (pk, val) in o.get_own_property_kvs(
            ctx,
            zuqe::JS_GPN_STR_OR_INT_MASK,
            zuqe::JSIteratorKindEnum::JS_ITERATOR_KIND_KEY_AND_VALUE,
        )? {
            if let Some(key) = pk.as_string(true)
                && let Some(val_str) = val.as_string(true)
            {
                headers.insert(
                    key.content().encode_utf8(false).to_string(),
                    val_str.content().encode_utf8(false).to_string(),
                );
            }
        }
    } else if !headers_val.is_undefined_or_null() {
        return Err(ctx.throw_type_error("headers must be an object"));
    }
    Ok(headers)
}
