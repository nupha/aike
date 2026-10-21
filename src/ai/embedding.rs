// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! Client for OpenAI-compatible embedding endpoints.
//!
//! Wire protocol: `POST {baseUrl}/embeddings` with a body of `{model, input}`
//! and a bearer token in `Authorization`. `baseUrl` carries the `/v1` prefix,
//! the same convention `LlmClient` uses for `{baseUrl}/chat/completions`. The
//! response carries `{object, data, model, usage}`; this client forwards the
//! parsed JSON verbatim, matching how `LlmClient` proxies the OpenAI API.
//!
//! There is no streaming: a call embeds the whole input in one pass and
//! returns a Promise.

use std::time::Duration;

use zuqe::{
    CallContext, ClassCtorDef, FieldDef, JSResult, JSValue, Object, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::whatwg::{
    abort::{signal_is_aborted, signal_notify, signal_reason},
    dom::make_dom_exception,
};

use super::{
    class_client::{
        ABORT_MSG, opt_secs_prop, opt_str_prop, parse_basic_call_opts, parse_headers_prop,
    },
    common::{
        ClientCommon, MAGIC_BASE_URL, MAGIC_HEADERS, MAGIC_MODEL, MAGIC_TIMEOUT,
        js_client_get_header, js_client_set_header, js_common_getter,
    },
};

/// The protocol defines exactly one inference endpoint.
const EMBEDDINGS_PATH: &str = "/embeddings";

#[derive(Debug)]
pub(crate) struct EmbeddingPayload {
    /// Connection config shared with the other `ai` clients (see `common.rs`).
    pub(crate) common: ClientCommon,
}

impl GcTrace for EmbeddingPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

fn get_client<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a EmbeddingPayload, i32> {
    JSEmbeddingClient::payload_of(val)
        .ok_or_else(|| ctx.throw_type_error("not an EmbeddingClient instance"))
}

/// Read a required non-empty string option. `undefined`, `null`, a non-string
/// and the empty string are all TypeError: there is no default to fall back to.
fn req_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<String, i32> {
    let v = obj.get_property_str(ctx, key)?;
    match v.as_string(true) {
        Some(s) if !s.content().encode_utf8(false).is_empty() => {
            Ok(s.content().encode_utf8(false).to_string())
        }
        _ => Err(ctx.throw_type_error(&format!("`{key}` must be a non-empty string"))),
    }
}

/// Constructor: new EmbeddingClient({baseUrl, model, apiKey?, timeout?, headers?})
///
/// No environment variable is read and no endpoint or model is assumed.
/// `baseUrl` and `model` are required: the caller names the endpoint the
/// request bills against and the model that produces the vectors.
fn js_client_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let opts = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("requires options object"))?;

    let mut base_url = req_str_prop(ctx, &opts, "baseUrl")?;
    let model = req_str_prop(ctx, &opts, "model")?;
    let api_key = opt_str_prop(ctx, &opts, "apiKey")?.unwrap_or_default();
    let timeout = opt_secs_prop(ctx, &opts, "timeout")?.unwrap_or(60);
    let headers = parse_headers_prop(ctx, &opts)?;

    // Strip trailing slashes so `${baseUrl}/embeddings` never doubles up.
    while base_url.ends_with('/') {
        base_url.pop();
    }

    let payload = EmbeddingPayload {
        common: ClientCommon {
            base_url,
            api_key,
            timeout: Duration::from_secs(timeout),
            headers,
            http: reqwest::Client::new(),
            model_name: model,
        },
    };

    JSEmbeddingClient::new_instance(ctx, new_target, payload).map(JSValue::from)
}

/// Convert the `input` argument to its wire form.
///
/// The protocol accepts a single string, an array of strings, an array of
/// token ids, or an array of token-id arrays. Anything else is a TypeError —
/// there is no implicit conversion.
fn input_to_wire(ctx: &mut ScopedContext, v: &JSValue) -> Result<serde_json::Value, i32> {
    if let Some(s) = v.as_string(true) {
        return Ok(serde_json::Value::String(
            s.content().encode_utf8(false).to_string(),
        ));
    }

    let Some(arr) = v.as_object_Array() else {
        return Err(ctx.throw_type_error(
            "`input` must be a string or an array of strings, numbers or number arrays",
        ));
    };

    let mut items = Vec::with_capacity(arr.payload().values().len());
    for item in arr.payload().values() {
        if let Some(s) = item.as_string(true) {
            items.push(serde_json::Value::String(
                s.content().encode_utf8(false).to_string(),
            ));
            continue;
        }
        if item.is_number() {
            let n = item.to_number(ctx)?;
            let v = match n {
                zuqe::IntOrFloat::Int64(i) => serde_json::Value::from(i),
                zuqe::IntOrFloat::Float64(d) => serde_json::Value::from(d),
            };
            items.push(v);
            continue;
        }
        // A token-id array: numbers only.
        let Some(ids) = item.as_object_Array() else {
            return Err(ctx.throw_type_error(
                "`input` must be a string or an array of strings, numbers or number arrays",
            ));
        };
        let mut id_list = Vec::with_capacity(ids.payload().values().len());
        for id in ids.payload().values() {
            if !id.is_number() {
                return Err(ctx.throw_type_error("`input` token ids must be numbers"));
            }
            let n = id.to_number(ctx)?;
            id_list.push(match n {
                zuqe::IntOrFloat::Int64(i) => serde_json::Value::from(i),
                zuqe::IntOrFloat::Float64(d) => serde_json::Value::from(d),
            });
        }
        items.push(serde_json::Value::Array(id_list));
    }

    Ok(serde_json::Value::Array(items))
}

/// Extract the server's error message from a failed response body.
///
/// OpenAI-compatible gateways report `{"error": {"message": "..."}}`; some
/// report a bare `{"error": "..."}` or `{"message": "..."}`.
fn extract_error_msg(body: &serde_json::Value) -> Option<String> {
    body.get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(String::from)
        .or_else(|| body.get("error").and_then(|e| e.as_str()).map(String::from))
        .or_else(|| {
            body.get("message")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
}

/// `client.embed({input, model?}, opts?)` → Promise
fn js_client_embed(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, &this)?;

    // arg(0): the request; arg(1): control-only opts.
    let req_obj = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("embed() requires a request object"))?;

    let input_val = req_obj.get_property_str(ctx, "input")?;
    if input_val.is_undefined() {
        return Err(ctx.throw_type_error("`input` is required"));
    }
    let input = input_to_wire(ctx, &input_val)?;

    let model = match opt_str_prop(ctx, &req_obj, "model")? {
        Some(m) => m,
        None => client.common.model_name.clone(),
    };

    let body = serde_json::json!({
        "model": model,
        "input": input,
    });

    let opts = parse_basic_call_opts(ctx, &cc)?;
    let abort_notify = signal_notify(&opts.signal);
    if signal_is_aborted(&opts.signal) {
        // Fail-fast: reject with the signal's reason without sending.
        let reason = match signal_reason(&opts.signal) {
            Some(r) => r,
            None => make_dom_exception(ctx, "AbortError", "The operation was aborted.")?,
        };
        return Promise::from_reject_value(ctx, reason).map(JSValue::from);
    }

    let http = client.common.http.clone();
    let api_key = client.common.api_key.clone();
    let headers = client.common.headers.clone();
    let url = format!("{}{}", client.common.base_url, EMBEDDINGS_PATH);
    // Per-request timeout (seconds) overrides the client-level default.
    let timeout = opts
        .timeout
        .map(Duration::from_secs)
        .unwrap_or(client.common.timeout);

    let mut req_builder = http
        .post(&url)
        .header("Content-Type", "application/json")
        .timeout(timeout)
        .body(serde_json::to_string(&body).unwrap_or_default());

    for (key, value) in &headers {
        req_builder = req_builder.header(key.as_str(), value.as_str());
    }
    if !api_key.is_empty() {
        req_builder = req_builder.header("Authorization", format!("Bearer {api_key}"));
    }

    let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), move |cx, data| -> JSResult {
            match data {
                Ok(resp) => JSValue::from_serde_json(cx, &resp),
                Err(e) => Err(cx.throw_internal_error(&e)),
            }
        })?;

    tokio::spawn(async move {
        // Abort races the whole request; dropping `result_fut` closes the
        // upstream connection.
        let result_fut = async {
            match req_builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    if !status.is_success() {
                        let msg = serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .as_ref()
                            .and_then(extract_error_msg)
                            .unwrap_or_else(|| format!("HTTP {status} : {text}"));
                        return Err(msg);
                    }
                    if text.is_empty() {
                        return Err(format!("HTTP {status} but returns empty body"));
                    }
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(json) => Ok(json),
                        Err(e) => {
                            let snippet = if text.len() > 200 {
                                &text[..200]
                            } else {
                                &text
                            };
                            Err(format!("Failed to parse JSON: {e} | body: {snippet}"))
                        }
                    }
                }
                Err(e) => Err(e.to_string()),
            }
        };

        let result = match abort_notify {
            Some(n) => tokio::select! {
                biased;
                _ = n.notified() => Err(ABORT_MSG.to_string()),
                r = result_fut => r,
            },
            None => result_fut.await,
        };
        let _ = result_slot.commit(result);
    });

    Ok(promise.into())
}

fn js_client_dispose(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let _ = JSEmbeddingClient::take_payload_of(&mut this);

    Ok(JSValue::UNDEFINED)
}

const CLASS_EMBEDDING_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("embed", 2, js_client_embed, 0),
    FieldDef::FUNC("dispose", 0, js_client_dispose, 0),
    FieldDef::FUNC("setHeader", 2, js_client_set_header, 0),
    FieldDef::FUNC("getHeader", 1, js_client_get_header, 0),
    // Every property this class exposes is shared with the other clients:
    // same magics, same getter.
    FieldDef::GETSET("model", Some(js_common_getter), None, MAGIC_MODEL as _),
    FieldDef::GETSET("baseUrl", Some(js_common_getter), None, MAGIC_BASE_URL as _),
    FieldDef::GETSET("timeout", Some(js_common_getter), None, MAGIC_TIMEOUT as _),
    FieldDef::GETSET("headers", Some(js_common_getter), None, MAGIC_HEADERS as _),
];

def_native_object!(
    "EmbeddingClient",
    JSEmbeddingClient,
    EmbeddingPayload,
    CLASS_EMBEDDING_PROTO,
    Some(ClassCtorDef::new(js_client_constructor, 1, false))
);
