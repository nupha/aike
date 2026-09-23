// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Notify;
use tokio_util::io::StreamReader;
use zuqe::{
    CallContext, ClassCtorDef, FieldDef, IntOrFloat, JS_PROP_WRITABLE, JSResult, JSValue, Object,
    ScopedContext,
    builtins::{
        iterator::{AsyncIterableIterator, AsyncIteratorFetcher, IterFetchStatus},
        promise::{Promise, PromiseResultSlot},
    },
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    helpers::json_keys_camel_to_snake,
    whatwg::{
        abort::{JSAbortSignal, signal_is_aborted, signal_notify, signal_reason},
        dom::make_dom_exception,
    },
};

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

/// Stream-consumer half of a `LlmClient.chat()`/`responses()` iterator.
///
/// Polls the result slot on the JS thread (`try_fetch`), applies the response
/// normalizer to yielded chunks, and forwards early `return()`/`break` to
/// [`StreamCancel`]. Not `Send`: it holds the `normalizer` JS value and lives
/// behind the engine's `Rc`.
pub(super) struct LlmStreamFetcher<D: Send> {
    slot: PromiseResultSlot<D>,
    cancel: Arc<StreamCancel>,
    normalizer: JSValue,
    convert: Box<dyn Fn(&mut ScopedContext, D) -> Result<Option<JSValue>, i32>>,
}

impl<D: Send + 'static> LlmStreamFetcher<D> {
    pub(super) fn new(
        slot: PromiseResultSlot<D>,
        cancel: Arc<StreamCancel>,
        normalizer: JSValue,
        convert: impl Fn(&mut ScopedContext, D) -> Result<Option<JSValue>, i32> + 'static,
    ) -> Self {
        Self {
            slot,
            cancel,
            normalizer,
            convert: Box::new(convert),
        }
    }
}

impl<D: Send + 'static> GcTrace for LlmStreamFetcher<D> {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        self.normalizer.trace(gcx);
    }
}

impl<D: Send + 'static> AsyncIteratorFetcher for LlmStreamFetcher<D> {
    fn try_fetch(&self, cx: &mut ScopedContext) -> Result<IterFetchStatus, i32> {
        let Some(data) = self.slot.try_take() else {
            return Ok(IterFetchStatus::Wait);
        };
        match (self.convert)(cx, data) {
            Ok(Some(v)) => {
                let v = if self.normalizer.is_callable() {
                    self.normalizer.call(cx, JSValue::UNDEFINED, &[v])?
                } else {
                    v
                };
                Ok(IterFetchStatus::Value(v))
            }
            Ok(None) => Ok(IterFetchStatus::Done),
            Err(e) => Err(e),
        }
    }

    fn close(&self) {
        self.cancel.cancel();
    }
}

use super::{
    MAGIC_CHAT_ONCE, MAGIC_CHAT_STREAM, MAGIC_RESPONSES_ONCE, MAGIC_RESPONSES_STREAM,
    OUTMSG_FIELD_CHOICES, OUTMSG_FIELD_REASONING, class_client_response::js_client_responses,
};

#[derive(Debug)]
pub(crate) struct ClientPayload {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: std::collections::HashMap<String, String>,
    pub(crate) timeout: Duration,
    /// Shared reqwest client: built once, reused across requests so the
    /// connection pool / keep-alive survives between calls.
    pub(crate) http: reqwest::Client,

    //
    // model params
    //
    pub(crate) model_name: String,
    pub(crate) max_tokens: usize,
    pub(crate) extra_body: serde_json::Value,
}

impl GcTrace for ClientPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

pub(super) fn get_client<'a>(
    ctx: &mut ScopedContext,
    val: &'a JSValue,
) -> Result<&'a ClientPayload, i32> {
    JSLlmClient::payload_of(val).ok_or_else(|| ctx.throw_type_error("not an LlmClient instance"))
}

pub(super) fn get_client_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut ClientPayload, i32> {
    JSLlmClient::payload_mut_of(val)
        .ok_or_else(|| ctx.throw_type_error("not an LlmClient instance"))
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
fn opt_secs_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<u64>, i32> {
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

/// Parse the optional control-opts second argument. `undefined` → all defaults;
/// anything but an object (including `null`) → TypeError.
pub(super) fn parse_call_opts(ctx: &mut ScopedContext, cc: &CallContext) -> Result<CallOpts, i32> {
    let v = cc.arg(1);
    if v.is_undefined() {
        return Ok(CallOpts {
            signal: JSValue::UNDEFINED,
            timeout: None,
            stallTimeout: None,
            coalesce_ms: 0,
        });
    }
    let obj = v
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("opts must be an object"))?;

    let signal = obj.get_property_str(ctx, "signal")?;
    if !signal.is_undefined_or_null() && JSAbortSignal::payload_of(&signal).is_none() {
        return Err(ctx.throw_type_error("`signal` must be an AbortSignal"));
    }

    // Coalesce window in ms: any integer; <= 0 disables coalescing.
    let coalesce_val = obj.get_property_str(ctx, "coalesce")?;
    let coalesce_ms = if coalesce_val.is_undefined() {
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

    Ok(CallOpts {
        signal,
        timeout: opt_secs_prop(ctx, &obj, "timeout")?,
        stallTimeout: opt_secs_prop(ctx, &obj, "stallTimeout")?,
        coalesce_ms,
    })
}

/// Constructor: new LlmClient({apiKey, baseUrl, model?, timeout?})
fn js_client_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // params from envvar
    let mut base_url = std::env::var("AIKE_LLM_BASE_URL").unwrap_or_default();
    let mut api_key = std::env::var("AIKE_LLM_API_KEY").unwrap_or_default();
    let mut model = std::env::var("AIKE_LLM_MODEL").unwrap_or_default();
    let mut max_tokens = if let Ok(s) = std::env::var("AIKE_LLM_MAX_TOKENS")
        && !s.is_empty()
    {
        if let Ok(n) = s.parse::<i32>()
            && n >= 0
        {
            n
        } else {
            0
        }
    } else {
        0
    };
    let mut timeout = 600; // sec
    let mut extra_body = {
        let s = std::env::var("AIKE_LLM_EXTRA_BODY").unwrap_or_default();
        if !s.is_empty() {
            if let Ok(o) = serde_json::from_str(&s) {
                o
            } else {
                eprintln!("[WARN] envvar AIKE_LLM_EXTRA_BODY is invalid JSON");
                serde_json::Value::Null
            }
        } else {
            serde_json::Value::Null
        }
    };
    let mut headers = std::collections::HashMap::new();
    let new_target = cc.this();

    let v = cc.arg(0);
    if !v.is_undefined() {
        let opts_obj = v
            .as_object()
            .ok_or_else(|| ctx.throw_type_error("requires options object"))?;

        let base_url_val = opts_obj.get_property_str(ctx, "baseUrl")?;
        if !base_url_val.is_undefined()
            && let Some(s) = base_url_val.as_string(true)
        {
            base_url = s.content().encode_utf8(false).to_string()
        }

        // apiKey
        let key_val = opts_obj.get_property_str(ctx, "apiKey")?;
        if let Some(s) = key_val
            .as_string(true)
            .map(|s| s.content().encode_utf8(false).to_string())
        {
            api_key = s
        }

        let model_val = opts_obj.get_property_str(ctx, "model")?;
        if !model_val.is_undefined()
            && let Some(s) = model_val
                .as_string(true)
                .map(|s| s.content().encode_utf8(false).to_string())
        {
            model = s;
        }

        let tokens_val = opts_obj.get_property_str(ctx, "maxTokens")?;
        if !tokens_val.is_undefined() {
            let n = v.to_number(ctx)?.to_i32();
            if n >= 0 {
                max_tokens = n;
            } else {
                return Err(ctx.throw_type_error("invalid maxTokens"));
            }
        }

        let timeout_val = opts_obj.get_property_str(ctx, "timeout")?;
        if !timeout_val.is_undefined() {
            let n = timeout_val.to_number(ctx).map(|n| n.to_i32())?;
            if n >= 0 {
                timeout = n;
            }
        }

        // extra_body
        let v = opts_obj.get_property_str(ctx, "extraBody")?;
        if let Some(o) = v.as_object() {
            extra_body = o.to_serde_json(ctx)?;
        } else if v.is_null() {
            extra_body = serde_json::Value::Null;
        } else if !v.is_undefined() {
            return Err(ctx.throw_type_error("extraBody must be one of undefined, null or object"));
        }

        debug_assert!(extra_body.is_null() || extra_body.is_object());

        // headers
        let headers_val = opts_obj.get_property_str(ctx, "headers")?;
        if let Some(mut obj) = headers_val.as_object() {
            for (pk, val) in obj.get_own_property_kvs(
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
    }

    let payload = ClientPayload {
        model_name: model,
        base_url,
        api_key,
        timeout: Duration::from_secs(timeout as u64),
        max_tokens: max_tokens as usize,
        extra_body,
        headers,
        http: reqwest::Client::new(),
    };

    JSLlmClient::new_instance(ctx, new_target, payload).and_then(|mut o| {
        // Initialize hidden internal property for responseNormalizer,
        // pre-created so the setter can use simple set_property_str.
        o.as_object_mut().define_property_value_str(
            ctx,
            INNER_PROP_RESPONSE_NORMALIZER,
            JSValue::NULL,
            JS_PROP_WRITABLE,
        )?;

        Ok(JSValue::from(o))
    })
}

/// Normalize message format in `choices[]` to standard shape.
/// For both streaming delta and non-streaming message.
fn normalize_message_shape(obj: &mut serde_json::Value) {
    if let Some(choices) = obj[OUTMSG_FIELD_CHOICES].as_array_mut() {
        for choice in choices.iter_mut() {
            // The target is either ‘delta’ (streaming) or ‘message’ (non-streaming).
            let target = match choice.as_object_mut() {
                Some(m) => {
                    if let Some(v) = m.get_mut("delta") {
                        v.as_object_mut()
                    } else if let Some(v) = m.get_mut("message") {
                        v.as_object_mut()
                    } else {
                        None
                    }
                }
                None => None,
            };

            if let Some(target) = target {
                // 1. fix reasoning → reasoning_content
                if let Some(val) = target.remove("reasoning")
                    && !target.contains_key(OUTMSG_FIELD_REASONING)
                {
                    target.insert(OUTMSG_FIELD_REASONING.to_string(), val);
                }
            }
        }
    }
}

/// Accumulates tool_call deltas until complete, then emits a single chunk.
struct ToolCallBuffer {
    idx: usize,
    id: Option<String>,
    r#type: Option<String>,
    name: Option<String>,
    arguments: String,
    // Original chunk metadata preserved for completeness
    chunk_id: Option<String>,
    chunk_object: Option<String>,
    chunk_created: Option<i64>,
    chunk_model: Option<String>,
}

impl ToolCallBuffer {
    fn new(idx: usize) -> Self {
        Self {
            idx,
            id: None,
            r#type: None,
            name: None,
            arguments: String::new(),
            chunk_id: None,
            chunk_object: None,
            chunk_created: None,
            chunk_model: None,
        }
    }

    fn apply_delta(&mut self, tc: &serde_json::Value, original: &serde_json::Value) {
        if let Some(id) = tc["id"].as_str() {
            self.id = Some(id.to_owned());
        }
        if let Some(t) = tc["type"].as_str() {
            self.r#type = Some(t.to_owned());
        }
        if let Some(name) = tc["function"]["name"].as_str() {
            self.name = Some(name.to_owned());
        }
        if let Some(args) = tc["function"]["arguments"].as_str() {
            self.arguments.push_str(args);
        }
        // Capture chunk-level metadata from the first chunk that triggers tool_call accumulation
        if self.chunk_id.is_none() {
            self.chunk_id = original["id"].as_str().map(String::from);
            self.chunk_object = original["object"].as_str().map(String::from);
            self.chunk_created = original["created"].as_i64();
            self.chunk_model = original["model"].as_str().map(String::from);
        }
    }

    fn into_chunk(self) -> serde_json::Value {
        serde_json::json!({
            "id": self.chunk_id,
            "object": self.chunk_object,
            "created": self.chunk_created,
            "model": self.chunk_model,
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": self.idx,
                        "id": self.id,
                        "type": self.r#type,
                        "function": {
                            "name": self.name,
                            "arguments": self.arguments,
                        }
                    }]
                },
                "finish_reason": null
            }]
        })
    }
}

enum ChatReply {
    Chunk(serde_json::Value), // Use raw JSON to preserve extension fields
    Done,
    Error(String),
}

/// Accumulates consecutive pure-content deltas into a single chunk. Active only
/// with the `coalesce` call opt: a fast (char-per-chunk) stream then amortizes
/// the per-chunk commit/wake/JS-convert cost over several deltas.
struct CoalescedChunk {
    id: Option<String>,
    object: Option<String>,
    created: Option<i64>,
    model: Option<String>,
    content: String,
    reasoning: String,
}

impl CoalescedChunk {
    fn new(json: &serde_json::Value) -> Self {
        Self {
            id: json["id"].as_str().map(String::from),
            object: json["object"].as_str().map(String::from),
            created: json["created"].as_i64(),
            model: json["model"].as_str().map(String::from),
            content: String::new(),
            reasoning: String::new(),
        }
    }

    fn apply_delta(&mut self, json: &serde_json::Value) {
        let delta = &json[OUTMSG_FIELD_CHOICES][0]["delta"];
        if let Some(c) = delta["content"].as_str() {
            self.content.push_str(c);
        }
        // Some servers emit "reasoning", others "reasoning_content".
        if let Some(r) = delta[OUTMSG_FIELD_REASONING].as_str() {
            self.reasoning.push_str(r);
        } else if let Some(r) = delta["reasoning"].as_str() {
            self.reasoning.push_str(r);
        }
    }

    fn into_chunk(self) -> serde_json::Value {
        let mut delta = serde_json::Map::new();
        if !self.content.is_empty() {
            delta.insert("content".into(), serde_json::Value::String(self.content));
        }
        if !self.reasoning.is_empty() {
            delta.insert(
                OUTMSG_FIELD_REASONING.into(),
                serde_json::Value::String(self.reasoning),
            );
        }
        serde_json::json!({
            "id": self.id,
            "object": self.object,
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": null
            }]
        })
    }
}

fn js_client_chatCompletions(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let client = get_client(ctx, &this)?;
    let is_stream = cc.magic() == MAGIC_CHAT_STREAM;

    // arg(0): the pure-data request body, serialized as-is into the API JSON.
    let req_val = cc.arg(0);
    let req_obj = req_val
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("chat() requires a request object"))?;

    // arg(1): control-only opts (signal/timeout/stallTimeout) — never submitted.
    let opts = parse_call_opts(ctx, &cc)?;
    let abort_notify = signal_notify(&opts.signal);
    let already_aborted = signal_is_aborted(&opts.signal);

    // Convert JS options to raw JSON, preserving all fields (including extension
    // fields like reasoning_content) that async-openai typed structs would drop.
    // LlmClient is a remote API proxy — it should forward everything, not filter.
    let mut req_json = req_obj.to_serde_json(ctx)?;

    // Per-request timeout (seconds), defaults to client-level timeout.
    // NOT sent to the API — controls HTTP timeout only.
    let req_timeout_secs = opts.timeout;

    // Per-request stream-stall timeout (seconds): abort when no stream data has
    // arrived for this long (the stream "stalled"). Also NOT sent to the API.
    let stall_timeout_secs = opts.stallTimeout;

    // camelCase → snake_case for OpenAI API compatibility
    json_keys_camel_to_snake(&mut req_json);

    if let Some(req) = req_json.as_object_mut() {
        // model
        if req.get("model").is_none_or(|v| !v.is_string()) {
            req.insert(
                "model".into(),
                serde_json::Value::String(client.model_name.to_string()),
            );
        }

        // max_tokens
        if req.get("max_tokens").is_none() && client.max_tokens > 0 {
            req.insert("max_tokens".into(), client.max_tokens.into());
        }

        // extra_body
        if let Some(b) = req.get("extra_body") {
            if let Some(req_body) = b.as_object() {
                if let Some(mut merged) = client.extra_body.as_object().cloned() {
                    // Merge default extra_body with per-request extra_body.
                    // LlmClient default extra_body serves as base, per-request fields override.
                    for (k, v) in req_body.iter() {
                        merged.insert(k.clone(), v.clone());
                    }
                    req.insert("extra_body".to_string(), serde_json::Value::Object(merged));
                }
            } else {
                return Err(ctx.throw_type_error("expect object: extraBody"));
            }
        } else if client.extra_body.is_object() {
            req.insert("extra_body".to_string(), client.extra_body.clone());
        }

        // Always tell the server explicitly whether to stream.
        req.insert("stream".into(), serde_json::Value::Bool(is_stream));

        if is_stream && !req.contains_key("stream_options") {
            let mut so = serde_json::Map::new();
            so.insert("include_usage".into(), serde_json::Value::Bool(true));
            req.insert("stream_options".into(), serde_json::Value::Object(so));
        }
    }

    // Use reqwest to make HTTP request, build client
    let http = client.http.clone();
    let mut req_builder = http
        .post(format!("{}/chat/completions", client.base_url))
        .body(serde_json::to_string(&req_json).unwrap_or_default())
        .header("Content-Type", "application/json")
        .timeout(client.timeout); // client default timeout

    // Apply custom headers (overrides Authorization if same key)
    for (key, value) in &client.headers {
        req_builder = req_builder.header(key.as_str(), value.as_str());
    }

    if !client.api_key.is_empty() {
        req_builder = req_builder.header("Authorization", format!("Bearer {}", client.api_key));
    }

    // Per-request timeout
    if let Some(secs) = req_timeout_secs {
        req_builder = req_builder.timeout(Duration::from_secs(secs));
    }

    // Read custom response normalizer from instance property
    let normalizer = if let Some(obj) = this.as_object() {
        obj.get_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER)?
    } else {
        JSValue::UNDEFINED
    };
    let has_normalizer = normalizer.is_callable();

    if is_stream {
        // Streaming: AsyncIterableIterator
        let coalesce_ms = opts.coalesce_ms;
        let result_slot = PromiseResultSlot::<ChatReply>::new();
        // Commits from the reading task wake the parked event loop so the
        // custom fetcher below polls the slot promptly.
        result_slot.attach_runtime_wake(ctx);
        let cancel = Arc::new(StreamCancel::new());
        let slot = result_slot.clone();
        let task_cancel = cancel.clone();

        tokio::spawn(async move {
            // `return()`/`break` on the JS iterator (`close()` on the fetcher)
            // fires `task_cancel`, dropping `body` — and with it the in-flight
            // response — so the upstream connection closes.
            let body = async {
                // Fail-fast: a pre-aborted signal never leaves the process.
                if already_aborted {
                    slot.commit_async(ChatReply::Error(ABORT_MSG.to_string()))
                        .await;
                    return;
                }

                async fn flush_buf(
                    slot: &PromiseResultSlot<ChatReply>,
                    buf: &mut Option<ToolCallBuffer>,
                ) {
                    if let Some(state) = buf.take() {
                        slot.commit_async(ChatReply::Chunk(state.into_chunk()))
                            .await;
                    }
                }

                async fn flush_acc(
                    slot: &PromiseResultSlot<ChatReply>,
                    acc: &mut Option<CoalescedChunk>,
                ) {
                    if let Some(c) = acc.take() {
                        slot.commit_async(ChatReply::Chunk(c.into_chunk())).await;
                    }
                }

                match req_builder.send().await {
                    Ok(response) => {
                        // Check HTTP status before reading SSE stream.
                        // non-2xx bodies are plain JSON, not SSE — they would be
                        // silently swallowed by the "data: " prefix filter otherwise.
                        let status = response.status();
                        if !status.is_success() {
                            let body = response.text().await.unwrap_or_default();
                            // Try to extract error message from OpenAI-style JSON body
                            let msg = serde_json::from_str::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|j| j["error"]["message"].as_str().map(String::from))
                                .unwrap_or_else(|| format!("HTTP {} : {}", status, body));
                            slot.commit_async(ChatReply::Error(msg)).await;
                            return;
                        }

                        // parse SSE manually
                        let byte_stream = response
                            .bytes_stream()
                            .map(|r| r.map_err(std::io::Error::other));

                        let reader = StreamReader::new(byte_stream);
                        let mut lines = tokio::io::BufReader::new(reader).lines();
                        let mut buf: Option<ToolCallBuffer> = None;

                        // Stall guard: wrap each frame read with a per-call timeout.
                        // tokio::time::timeout measures the gap between consecutive
                        // SSE lines, so any received byte naturally resets it. When
                        // it fires, an Error is committed and the buffered
                        // tool-call fragment is intentionally dropped (stalled
                        // partial output is worthless).
                        let mut stall_aborted = false;
                        let stall_limit =
                            stall_timeout_secs.map(|s| std::time::Duration::from_secs(s));
                        let notify = abort_notify.clone();

                        // Accumulated coalesced content chunk and a line stashed
                        // by the coalesce window for the main loop to process.
                        let mut acc: Option<CoalescedChunk> = None;
                        let mut stash: Option<String> = None;

                        'sse: loop {
                            // Fast path: no abort signal and no stall guard —
                            // read the next line directly, skipping the per-line
                            // select!/timeout scaffolding.
                            let next_result: std::io::Result<Option<String>> = if let Some(
                                stashed,
                            ) = stash.take()
                            {
                                Ok(Some(stashed))
                            } else if notify.is_none() && stall_limit.is_none() {
                                lines.next_line().await
                            } else {
                                // Line read raced against the stall timeout and the user
                                // abort signal. `biased` lets a fired signal win even when
                                // a line is simultaneously ready.
                                let line_fut = async {
                                    match stall_limit {
                                        Some(stall) => {
                                            tokio::time::timeout(stall, lines.next_line())
                                                .await
                                                .map_err(|_| ())
                                        }
                                        None => Ok(lines.next_line().await),
                                    }
                                };
                                let next = match &notify {
                                    Some(n) => tokio::select! {
                                        biased;
                                        _ = n.notified() => {
                                            slot.commit_async(ChatReply::Error(ABORT_MSG.to_string())).await;
                                            break 'sse;
                                        }
                                        r = line_fut => r,
                                    },
                                    None => line_fut.await,
                                };
                                match next {
                                    Ok(inner) => inner,
                                    Err(()) => {
                                        stall_aborted = true;
                                        slot.commit_async(ChatReply::Error(format!(
                                            "llm stream stall timeout: no data for {}s",
                                            stall_limit.map(|d| d.as_secs()).unwrap_or(0)
                                        )))
                                        .await;
                                        break 'sse;
                                    }
                                }
                            };
                            // Preserve legacy end semantics: Ok(Some)=frame,
                            // Ok(None)=EOF, Err=end silently.
                            let Ok(Some(line)) = next_result else {
                                break 'sse;
                            };
                            let line = line.trim();
                            let Some(data) = line.strip_prefix("data: ") else {
                                continue;
                            };
                            if data == "[DONE]" {
                                flush_acc(&slot, &mut acc).await;
                                if buf.is_some() {
                                    flush_buf(&slot, &mut buf).await;
                                }
                                break;
                            }

                            let Ok(mut json) = serde_json::from_str::<serde_json::Value>(data)
                            else {
                                continue;
                            };
                            if json.get("error").is_some() {
                                flush_acc(&slot, &mut acc).await;
                                if buf.is_some() {
                                    flush_buf(&slot, &mut buf).await;
                                }

                                let err_msg = json["error"]["message"]
                                    .as_str()
                                    .unwrap_or(data)
                                    .to_string();
                                slot.commit_async(ChatReply::Error(err_msg)).await;
                                break;
                            }

                            // Check for tool_calls in this chunk
                            if let Some(tcs) =
                                json[OUTMSG_FIELD_CHOICES][0]["delta"]["tool_calls"].as_array()
                                && !tcs.is_empty()
                            {
                                flush_acc(&slot, &mut acc).await;
                                // A chunk may carry content/reasoning alongside tool_calls.
                                // Since we only buffer tool_calls here, emit that text now —
                                // otherwise it is lost when finish_reason arrives in a later,
                                // content-less chunk.
                                let has_finish = json[OUTMSG_FIELD_CHOICES][0]["finish_reason"]
                                    .as_str()
                                    .is_some();
                                let delta = &json[OUTMSG_FIELD_CHOICES][0]["delta"];
                                if !has_finish
                                    && (delta.get("content").is_some()
                                        || delta.get("reasoning_content").is_some())
                                {
                                    let mut text_chunk = json.clone();
                                    if let Some(obj) =
                                        text_chunk[OUTMSG_FIELD_CHOICES][0]["delta"].as_object_mut()
                                    {
                                        obj.remove("tool_calls");
                                    }
                                    if !has_normalizer {
                                        normalize_message_shape(&mut text_chunk);
                                    }
                                    slot.commit_async(ChatReply::Chunk(text_chunk)).await;
                                }

                                // Accumulate tool_call deltas
                                for tc in tcs {
                                    let idx = tc["index"].as_i64().unwrap_or(0) as usize;
                                    // Got a new index → flush previous buffered tool_call (matches openai-node logic)
                                    if let Some(ref cur) = buf
                                        && cur.idx != idx
                                    {
                                        flush_buf(&slot, &mut buf).await;
                                    }

                                    let state = buf.get_or_insert_with(|| ToolCallBuffer::new(idx));
                                    state.apply_delta(tc, &json);
                                }

                                // check finish_reason AFTER processing tool_calls
                                // (non-standard servers may emit both in one chunk)
                                if json[OUTMSG_FIELD_CHOICES][0]["finish_reason"]
                                    .as_str()
                                    .is_some()
                                {
                                    flush_buf(&slot, &mut buf).await;

                                    // MUST NOT emit the original json as-is: it still carries the last
                                    // tool_calls delta. Emit a clean finish-only chunk instead.
                                    let mut finish = json.clone();
                                    if let Some(obj) =
                                        finish[OUTMSG_FIELD_CHOICES][0]["delta"].as_object_mut()
                                    {
                                        obj.remove("tool_calls");
                                    }
                                    // Normalize message format in choices[] delta
                                    if !has_normalizer {
                                        normalize_message_shape(&mut finish);
                                    }
                                    slot.commit_async(ChatReply::Chunk(finish)).await;
                                }
                                continue;
                            }

                            // Non-tool_call (content, finish_reason, etc.) → flush
                            // then passthrough.
                            flush_buf(&slot, &mut buf).await;

                            // Coalesce pure-content deltas when enabled: accumulate
                            // and keep pulling lines within a short idle window
                            // before committing one merged chunk. Non-mergable
                            // lines are stashed for the next loop iteration.
                            let delta = &json[OUTMSG_FIELD_CHOICES][0]["delta"];
                            let is_pure_content = json[OUTMSG_FIELD_CHOICES][0]["finish_reason"]
                                .as_str()
                                .is_none()
                                && (delta.get("content").is_some()
                                    || delta.get("reasoning_content").is_some()
                                    || delta.get("reasoning").is_some());
                            if coalesce_ms > 0 && is_pure_content {
                                let a = acc.get_or_insert_with(|| CoalescedChunk::new(&json));
                                a.apply_delta(&json);
                                let window = std::time::Duration::from_millis(coalesce_ms);
                                loop {
                                    match tokio::time::timeout(window, lines.next_line()).await {
                                        Ok(Ok(Some(next))) => {
                                            let next = next.trim();
                                            // SSE frames are separated by blank
                                            // lines — skip them, keep coalescing.
                                            if next.is_empty() {
                                                continue;
                                            }
                                            let Some(d) = next.strip_prefix("data: ") else {
                                                stash = Some(next.to_string());
                                                flush_acc(&slot, &mut acc).await;
                                                break;
                                            };
                                            let Ok(j) =
                                                serde_json::from_str::<serde_json::Value>(d)
                                            else {
                                                stash = Some(next.to_string());
                                                flush_acc(&slot, &mut acc).await;
                                                break;
                                            };
                                            let nd = &j[OUTMSG_FIELD_CHOICES][0]["delta"];
                                            let npure = j[OUTMSG_FIELD_CHOICES][0]["finish_reason"]
                                                .as_str()
                                                .is_none()
                                                && (nd.get("content").is_some()
                                                    || nd.get("reasoning_content").is_some()
                                                    || nd.get("reasoning").is_some());
                                            if !npure {
                                                stash = Some(next.to_string());
                                                flush_acc(&slot, &mut acc).await;
                                                break;
                                            }
                                            let a =
                                                acc.get_or_insert_with(|| CoalescedChunk::new(&j));
                                            a.apply_delta(&j);
                                        }
                                        _ => {
                                            flush_acc(&slot, &mut acc).await;
                                            break;
                                        }
                                    }
                                }
                                continue;
                            }

                            // Normalize message format in choices[] delta
                            flush_acc(&slot, &mut acc).await;
                            if !has_normalizer {
                                normalize_message_shape(&mut json);
                            }
                            slot.commit_async(ChatReply::Chunk(json)).await;
                        }

                        // Stream ended without [DONE] (server closed early): flush
                        // any still-buffered tool_call so it is not silently lost.
                        // On the normal [DONE] path the buffers are already empty.
                        // Skipped when the stall guard aborted the stream.
                        if !stall_aborted {
                            flush_acc(&slot, &mut acc).await;
                            if buf.is_some() {
                                flush_buf(&slot, &mut buf).await;
                            }
                            slot.commit_async(ChatReply::Done).await;
                        }
                    }
                    Err(e) => {
                        slot.commit_async(ChatReply::Error(e.to_string())).await;
                    }
                }
            };

            tokio::select! {
                biased;
                _ = task_cancel.notified() => {}
                _ = body => {}
            }
        });

        AsyncIterableIterator::create(
            ctx,
            LlmStreamFetcher::new(
                result_slot,
                cancel,
                normalizer,
                |cx, msg| -> Result<Option<JSValue>, i32> {
                    match msg {
                        ChatReply::Chunk(chunk) => {
                            let val = JSValue::from_serde_json(cx, &chunk)?;
                            Ok(Some(val))
                        }
                        ChatReply::Done => Ok(None),
                        ChatReply::Error(e) => Err(cx.throw_internal_error(&e)),
                    }
                },
            ),
        )
        .map(JSValue::from)
    } else {
        // Non-streaming: Promise - Use reqwest to preserve extension fields
        if already_aborted {
            // Fail-fast: reject with the signal's reason without sending.
            let reason = match signal_reason(&opts.signal) {
                Some(r) => r,
                None => make_dom_exception(ctx, "AbortError", "The operation was aborted.")?,
            };
            return Promise::from_reject_value(ctx, reason).map(JSValue::from);
        }

        let result_slot = PromiseResultSlot::<Result<serde_json::Value, String>>::new();
        let normalizer_ns = normalizer;
        let promise = Promise::create_with_result_slot(
            ctx,
            result_slot.clone(),
            move |cx, data| -> JSResult {
                match data {
                    Ok(resp) => {
                        let val = JSValue::from_serde_json(cx, &resp)?;
                        let val = if normalizer_ns.is_callable() {
                            normalizer_ns.call(cx, JSValue::UNDEFINED, &[val])?
                        } else {
                            val
                        };
                        Ok(val)
                    }
                    Err(e) => Err(cx.throw_internal_error(&e)),
                }
            },
        )?;

        tokio::spawn(async move {
            // Abort races the whole request; dropping `result_fut` closes the
            // upstream connection, so generation stops server-side too.
            let result_fut = async {
                let result = match req_builder.send().await {
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();

                        if status.is_success() {
                            if body.is_empty() {
                                // HTTP success, but empty response body -> upstream failure
                                Err(format!("HTTP {} but returns empty body", status))
                            } else {
                                match serde_json::from_str::<serde_json::Value>(&body) {
                                    Ok(mut json) => {
                                        // Check for error in response
                                        if json.get("error").is_some() {
                                            let err_msg = json["error"]["message"]
                                                .as_str()
                                                .unwrap_or(&body)
                                                .to_string();
                                            Err(err_msg)
                                        } else {
                                            // Normalize message format in choices[] message
                                            if !has_normalizer {
                                                normalize_message_shape(&mut json);
                                            }
                                            Ok(json)
                                        }
                                    }
                                    Err(e) => {
                                        let snippet = if body.len() > 200 {
                                            &body[..200]
                                        } else {
                                            &body
                                        };
                                        Err(format!(
                                            "Failed to parse JSON: {} | body: {}",
                                            e, snippet
                                        ))
                                    }
                                }
                            }
                        } else {
                            Err(format!("HTTP {} : {}", status, body))
                        }
                    }
                    Err(e) => Err(e.to_string()),
                };
                result
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
}

/// Getter magic constants
const PROP_MODEL: i32 = 0;
const PROP_RESPONSE_NORMALIZER: i32 = 1;
const PROP_BASE_URL: i32 = 2;
const PROP_EXTRA_BODY: i32 = 4;
const PROP_TIMEOUT: i32 = 5;
const PROP_MAX_TOKENS: i32 = 6;
const PROP_HEADERS: i32 = 7;

/// Internal property key for responseNormalizer, hidden from JS enumeration
const INNER_PROP_RESPONSE_NORMALIZER: &str = "_responseNormalizer";

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
        PROP_MAX_TOKENS => Ok(JSValue::from(client.max_tokens as i32)),
        PROP_EXTRA_BODY => {
            if client.extra_body.is_null() {
                Ok(JSValue::NULL)
            } else {
                JSValue::from_serde_json(ctx, &client.extra_body)
            }
        }
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

fn js_client_get_normalizer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let val = if let Some(obj) = this.as_object() {
        obj.get_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER)?
    } else {
        JSValue::UNDEFINED
    };
    Ok(val)
}

fn js_client_set_normalizer(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let val = cc.arg(0);
    if !val.is_null() && !val.is_callable() {
        return Err(ctx.throw_internal_error("responseNormalizer must be a function or null"));
    }
    if let Some(mut obj) = this.as_object() {
        obj.set_property_str(ctx, INNER_PROP_RESPONSE_NORMALIZER, val)?;
    }
    Ok(JSValue::UNDEFINED)
}

fn js_client_set_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let name_val = cc.arg(0);
    let val = cc.arg(1);

    let name = match name_val.as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => {
            return Err(ctx.throw_type_error("setHeader: header name must be a string"));
        }
    };

    let payload = get_client_mut(ctx, &mut this)?;

    // Undefined → no-op (matches the bulk setter's no-op semantics).
    if val.is_undefined() {
        return Ok(JSValue::UNDEFINED);
    }
    // Null → delete the entry.
    if val.is_null() {
        payload.headers.remove(&name);
        return Ok(JSValue::UNDEFINED);
    }
    // String → insert/update.
    let s = match val.as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => {
            return Err(
                ctx.throw_type_error("setHeader: value must be a string, null, or undefined")
            );
        }
    };
    payload.headers.insert(name, s);
    Ok(JSValue::UNDEFINED)
}

fn js_client_get_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let name_val = cc.arg(0);

    let name = match name_val.as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => {
            return Err(ctx.throw_type_error("getHeader: header name must be a string"));
        }
    };

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
    let _ = JSLlmClient::take_payload_of(&mut this);

    Ok(JSValue::UNDEFINED)
}

const CLASS_CLIENT_PROTO: &[FieldDef] = &[
    FieldDef::FUNC(
        "chat",
        2,
        js_client_chatCompletions,
        MAGIC_CHAT_STREAM as i16,
    ),
    FieldDef::FUNC(
        "chatOnce",
        2,
        js_client_chatCompletions,
        MAGIC_CHAT_ONCE as i16,
    ),
    FieldDef::FUNC(
        "responses",
        2,
        js_client_responses,
        MAGIC_RESPONSES_STREAM as i16,
    ),
    FieldDef::FUNC(
        "responsesOnce",
        2,
        js_client_responses,
        MAGIC_RESPONSES_ONCE as i16,
    ),
    FieldDef::FUNC("dispose", 0, js_client_dispose, 0),
    FieldDef::FUNC("setHeader", 2, js_client_set_header, 0),
    FieldDef::FUNC("getHeader", 1, js_client_get_header, 0),
    FieldDef::GETSET("model", Some(js_client_getter), None, PROP_MODEL as _),
    FieldDef::GETSET("baseUrl", Some(js_client_getter), None, PROP_BASE_URL as _),
    FieldDef::GETSET(
        "maxTokens",
        Some(js_client_getter),
        None,
        PROP_MAX_TOKENS as _,
    ),
    FieldDef::GETSET(
        "extraBody",
        Some(js_client_getter),
        None,
        PROP_EXTRA_BODY as _,
    ),
    FieldDef::GETSET("timeout", Some(js_client_getter), None, PROP_TIMEOUT as _),
    FieldDef::GETSET("headers", Some(js_client_getter), None, PROP_HEADERS as _),
    FieldDef::GETSET(
        "responseNormalizer",
        Some(js_client_get_normalizer),
        Some(js_client_set_normalizer),
        PROP_RESPONSE_NORMALIZER as _,
    ),
];

def_native_object!(
    "LlmClient",
    JSLlmClient,
    ClientPayload,
    CLASS_CLIENT_PROTO,
    Some(ClassCtorDef::new(js_client_constructor, 1, false))
);
