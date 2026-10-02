// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Client for TypeSafe's System One Models (the Jev family).
//!
//! Wire protocol: `POST {baseUrl}/v1/systemone` with a body of
//! `{model, state, questions}` and a bearer token in `Authorization`. The
//! response carries `{model, answers, usage}`; this client forwards the parsed
//! JSON verbatim, matching how `LlmClient` proxies the OpenAI API.
//!
//! Unlike chat completions there is no text generation and no streaming: a
//! call answers every question in one pass and returns a Promise.

use std::{collections::HashMap, time::Duration};

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

use super::class_client::{
    ABORT_MSG, opt_secs_prop, opt_str_prop, parse_basic_call_opts, parse_headers_prop,
};

/// The protocol defines exactly one inference endpoint.
const SYSTEMONE_PATH: &str = "/v1/systemone";
/// The three question kinds a System One model answers.
const QUESTION_TYPES: &[&str] = &["choice", "score", "noul"];

/// Getter magic constants.
const PROP_MODEL: i32 = 0;
const PROP_BASE_URL: i32 = 1;
const PROP_TIMEOUT: i32 = 2;
const PROP_HEADERS: i32 = 3;

#[derive(Debug)]
pub(crate) struct System1Payload {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) timeout: Duration,
    /// Shared reqwest client: built once so the connection pool / keep-alive
    /// survives between calls.
    pub(crate) http: reqwest::Client,
    pub(crate) model_name: String,
}

impl GcTrace for System1Payload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

fn get_client<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a System1Payload, i32> {
    JSSystem1Client::payload_of(val)
        .ok_or_else(|| ctx.throw_type_error("not a System1Client instance"))
}

fn get_client_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut System1Payload, i32> {
    JSSystem1Client::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not a System1Client instance"))
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

/// Constructor: new System1Client({baseUrl, model, apiKey?, timeout?, headers?})
///
/// No environment variable is read and no endpoint or model is assumed.
/// `baseUrl` and `model` are required, so the caller is explicit about which
/// endpoint a request bills against and which model answers it — pinning the
/// model matters because a versionless alias can move under your feet and
/// change every answer. `apiKey` is optional: omitting it sends no
/// `Authorization` header, which is what a keyless local gateway wants.
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

    // Strip trailing slashes so `${baseUrl}/v1/systemone` never doubles up.
    while base_url.ends_with('/') {
        base_url.pop();
    }

    let payload = System1Payload {
        base_url,
        api_key,
        timeout: Duration::from_secs(timeout),
        headers,
        http: reqwest::Client::new(),
        model_name: model,
    };

    JSSystem1Client::new_instance(ctx, new_target, payload).map(JSValue::from)
}

/// Validate one question object against the protocol's per-type rules.
///
/// Strict: a wrong shape is a `TypeError`, a right type with an out-of-range
/// value (an unknown `type`, too few score levels) is a `RangeError`.
fn validate_question(
    ctx: &mut ScopedContext,
    name: &str,
    q_val: &JSValue,
) -> Result<serde_json::Value, i32> {
    let Some(q) = q_val.as_object() else {
        return Err(ctx.throw_type_error(&format!("`questions.{name}` must be an object")));
    };

    let type_val = q.get_property_str(ctx, "type")?;
    let type_str = type_val
        .as_string(true)
        .ok_or_else(|| ctx.throw_type_error(&format!("`questions.{name}.type` must be a string")))?
        .content()
        .encode_utf8(false)
        .to_string();

    if !QUESTION_TYPES.contains(&type_str.as_str()) {
        return Err(ctx.throw_range_error(&format!(
            "`questions.{name}.type` must be one of: {}",
            QUESTION_TYPES.join(", ")
        )));
    }

    let instructions = q.get_property_str(ctx, "instructions")?;
    let criteria = q.get_property_str(ctx, "criteria")?;

    // A question is pure data: `instructions` is text/object/array/null or
    // absent. Anything else has no wire representation.
    if !instructions.is_undefined() && !is_entry(&instructions) {
        return Err(ctx.throw_type_error(&format!(
            "`questions.{name}.instructions` must be a string, object, array or null"
        )));
    }

    let mut out = serde_json::Map::new();
    out.insert("type".into(), serde_json::Value::String(type_str.clone()));

    match type_str.as_str() {
        "choice" => {
            // Labels mapped to descriptions. A list is a score rubric, not a choice.
            if criteria.is_undefined() || criteria.is_null() || criteria.as_object_Array().is_some()
            {
                return Err(ctx.throw_type_error(&format!(
                    "`questions.{name}.criteria` must be an object of label to description"
                )));
            }
            out.insert("criteria".into(), criteria.to_serde_json(ctx)?);
        }
        "score" => {
            // An ordered rubric: at least two levels, indexed from zero.
            let Some(arr) = criteria.as_object_Array() else {
                return Err(ctx.throw_type_error(&format!(
                    "`questions.{name}.criteria` must be an array of level descriptions"
                )));
            };
            if arr.payload().len() < 2 {
                return Err(ctx.throw_range_error(&format!(
                    "`questions.{name}.criteria` must have at least two levels"
                )));
            }
            out.insert("criteria".into(), criteria.to_serde_json(ctx)?);
        }
        // noul: `criteria` optionally describes the true/false outcomes. An
        // omitted `criteria` stays omitted on the wire, matching the SDK.
        _ if criteria.is_undefined() => {}
        _ if !criteria.is_null() && criteria.as_object().is_none() => {
            return Err(ctx.throw_type_error(&format!(
                "`questions.{name}.criteria` must be an object with `true`/`false` descriptions"
            )));
        }
        _ => {
            out.insert("criteria".into(), criteria.to_serde_json(ctx)?);
        }
    }

    if !instructions.is_undefined() {
        out.insert("instructions".into(), instructions.to_serde_json(ctx)?);
    }

    Ok(serde_json::Value::Object(out))
}

/// Extract the server's error message from a failed response body.
///
/// Mirrors the SDK's `extractMessage`: transport problems arrive under `error`,
/// request-validation problems under `detail`, which is an array of
/// `{loc, msg}` entries joined into `path: message` pairs.
fn extract_error_msg(body: &serde_json::Value) -> Option<String> {
    let as_text = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(String::from)
            .or_else(|| v["message"].as_str().map(String::from))
    };
    if let Some(detail) = body.get("detail")
        && let Some(items) = detail.as_array()
    {
        let parts: Vec<String> = items
            .iter()
            .filter_map(|e| {
                let msg = e["msg"].as_str()?;
                // `body` is the implicit root, not part of the path to report.
                let loc: Vec<&str> = e["loc"]
                    .as_array()?
                    .iter()
                    .filter_map(|s| s.as_str())
                    .filter(|s| *s != "body")
                    .collect();
                Some(if loc.is_empty() {
                    msg.to_string()
                } else {
                    format!("{}: {msg}", loc.join("."))
                })
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("; "));
        }
    }
    as_text(&body["error"])
        .or_else(|| body["message"].as_str().map(String::from))
        .or_else(|| as_text(&body["detail"]))
}

/// `client.systemOne({state, questions, model?}, opts?)` → Promise
fn js_client_system_one(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, &this)?;

    // arg(0): the pure-data request; arg(1): control-only opts.
    let req_obj = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("systemOne() requires a request object"))?;

    let state_val = req_obj.get_property_str(ctx, "state")?;
    if state_val.is_undefined() {
        return Err(ctx.throw_type_error("`state` is required"));
    }
    if !is_entry(&state_val) {
        return Err(ctx.throw_type_error("`state` must be a string, object, array or null"));
    }
    let state = state_val.to_serde_json(ctx)?;

    let model = match opt_str_prop(ctx, &req_obj, "model")? {
        Some(m) => m,
        None => client.model_name.clone(),
    };

    let mut questions_obj = req_obj
        .get_property_str(ctx, "questions")?
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("`questions` must be an object"))?;
    let mut questions = serde_json::Map::new();
    let entries = questions_obj.get_own_property_kvs(
        ctx,
        zuqe::JS_GPN_STR_OR_INT_MASK,
        zuqe::JSIteratorKindEnum::JS_ITERATOR_KIND_KEY_AND_VALUE,
    )?;
    if entries.is_empty() {
        return Err(ctx.throw_type_error("`questions` must not be empty"));
    }
    for (name, q) in entries {
        let name = name
            .as_string(true)
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default();
        questions.insert(name.clone(), validate_question(ctx, &name, &q)?);
    }

    let body = serde_json::json!({
        "model": model,
        "state": state,
        "questions": serde_json::Value::Object(questions),
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

    let http = client.http.clone();
    let api_key = client.api_key.clone();
    let headers = client.headers.clone();
    let url = format!("{}{}", client.base_url, SYSTEMONE_PATH);
    // Per-request timeout (seconds) overrides the client-level default.
    let timeout = opts
        .timeout
        .map(Duration::from_secs)
        .unwrap_or(client.timeout);

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

fn js_client_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, &this)?;

    match cc.magic() {
        PROP_MODEL => ctx
            .create_string_from_utf8(client.model_name.as_bytes().to_vec())
            .map(JSValue::make_string),
        PROP_BASE_URL => ctx
            .create_string_from_utf8(client.base_url.as_bytes().to_vec())
            .map(JSValue::make_string),
        PROP_TIMEOUT => Ok(JSValue::from(client.timeout.as_secs() as i32)),
        PROP_HEADERS => {
            let mut obj = ctx.create_base_object()?;
            for (k, v) in &client.headers {
                let val = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
                obj.set_property_str(ctx, k, JSValue::make_string(val))?;
            }
            Ok(JSValue::from(obj))
        }
        _ => Ok(JSValue::UNDEFINED),
    }
}

fn js_client_set_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let name = opt_str_arg(ctx, cc.arg(0), "setHeader: header name")?;

    let payload = get_client_mut(ctx, &mut this)?;

    // Undefined → no-op (matches the bulk setter's no-op semantics).
    let val = cc.arg(1);
    if val.is_undefined() {
        return Ok(JSValue::UNDEFINED);
    }
    // Null → delete the entry.
    if val.is_null() {
        payload.headers.remove(&name);
        return Ok(JSValue::UNDEFINED);
    }
    // String → insert/update.
    match val.as_string(true) {
        Some(s) => {
            payload
                .headers
                .insert(name, s.content().encode_utf8(false).to_string());
            Ok(JSValue::UNDEFINED)
        }
        None => Err(ctx.throw_type_error("setHeader: value must be a string, null, or undefined")),
    }
}

/// Read a required string argument of a header accessor.
fn opt_str_arg(ctx: &mut ScopedContext, v: JSValue, label: &str) -> Result<String, i32> {
    v.as_string(true)
        .map(|s| s.content().encode_utf8(false).to_string())
        .ok_or_else(|| ctx.throw_type_error(&format!("{label} must be a string")))
}

fn js_client_get_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let name = opt_str_arg(ctx, cc.arg(0), "getHeader: header name")?;

    let payload = get_client(ctx, &this)?;
    match payload.headers.get(&name) {
        Some(v) => ctx
            .create_string_from_utf8(v.as_bytes().to_vec())
            .map(JSValue::make_string),
        None => Ok(JSValue::UNDEFINED),
    }
}

fn js_client_dispose(_: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let _ = JSSystem1Client::take_payload_of(&mut this);

    Ok(JSValue::UNDEFINED)
}

/// `choice(instructions, criteria)` — one named option out of a set.
pub(crate) fn js_question_choice(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    build_question(ctx, &cc, "choice")
}

/// `score(instructions, criteria)` — a position on an ordered rubric.
pub(crate) fn js_question_score(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    build_question(ctx, &cc, "score")
}

/// `noul(instructions, criteria)` — a yes/no probability.
pub(crate) fn js_question_noul(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    build_question(ctx, &cc, "noul")
}

/// True when `v` is one of the protocol's "entry" shapes: text, a JSON object,
/// an array, or `null`. Numbers, booleans, functions and symbols have no wire
/// representation and are type errors rather than being coerced.
fn is_entry(v: &JSValue) -> bool {
    v.is_null() || v.as_object().is_some() || v.as_string(true).is_some()
}

/// Build the wire object for one question. Exported so JS can write questions
/// the same way the official SDK does.
fn build_question(ctx: &mut ScopedContext, cc: &CallContext, kind: &str) -> JSResult {
    let instructions = cc.arg(0);
    if !instructions.is_undefined() && !is_entry(&instructions) {
        return Err(ctx.throw_type_error("`instructions` must be a string, object, array or null"));
    }
    let criteria = cc.arg(1);
    match kind {
        // An ordered rubric; two levels minimum.
        "score" if criteria.as_object_Array().is_none() => {
            return Err(ctx.throw_type_error("`criteria` must be an array of level descriptions"));
        }
        // Labels mapped to descriptions. A list is a rubric, not a choice.
        "choice" if criteria.is_undefined() || criteria.as_object_Array().is_some() => {
            return Err(
                ctx.throw_type_error("`criteria` must be an object of label to description")
            );
        }
        // noul: criteria is optional and describes the true/false outcomes.
        "noul"
            if !criteria.is_undefined()
                && !criteria.is_null()
                && criteria.as_object().is_none() =>
        {
            return Err(ctx.throw_type_error(
                "`criteria` must be an object with `true`/`false` descriptions",
            ));
        }
        _ => {}
    }

    let mut obj = ctx.create_base_object()?;
    let type_str = ctx.create_string_from_utf8(kind.as_bytes().to_vec())?;
    obj.set_property_str(ctx, "type", JSValue::make_string(type_str))?;
    obj.set_property_str(ctx, "instructions", instructions)?;
    obj.set_property_str(ctx, "criteria", criteria)?;
    Ok(obj.into())
}

const CLASS_SYSTEM1_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("systemOne", 2, js_client_system_one, 0),
    FieldDef::FUNC("dispose", 0, js_client_dispose, 0),
    FieldDef::FUNC("setHeader", 2, js_client_set_header, 0),
    FieldDef::FUNC("getHeader", 1, js_client_get_header, 0),
    FieldDef::GETSET("model", Some(js_client_getter), None, PROP_MODEL as _),
    FieldDef::GETSET("baseUrl", Some(js_client_getter), None, PROP_BASE_URL as _),
    FieldDef::GETSET("timeout", Some(js_client_getter), None, PROP_TIMEOUT as _),
    FieldDef::GETSET("headers", Some(js_client_getter), None, PROP_HEADERS as _),
];

def_native_object!(
    "System1Client",
    JSSystem1Client,
    System1Payload,
    CLASS_SYSTEM1_PROTO,
    Some(ClassCtorDef::new(js_client_constructor, 1, false))
);
