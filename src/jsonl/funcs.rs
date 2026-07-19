// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};

use serde_json::Value as JsonValue;

use zuqe::{
    CallContext, JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSResult, JSValue, PollSender, ScopedContext,
    builtins::{
        iterator::{AsyncIterableIterator, AsyncIteratorFetcher, IterFetchStatus},
        promise::Promise,
        typed_array::TypedArray,
    },
};

use super::class_jsonl_array::{JSJSONLArray, jsonl_array_stringify, make_jsonl_array};

/// How to handle malformed lines.
#[derive(Clone, Copy)]
enum OnError {
    Skip,
    Throw,
    Replace,
}

/// Read onError / failsafe from options.
fn extract_opts(cx: &mut ScopedContext, opts_val: JSValue) -> Result<(OnError, JSValue), i32> {
    let mut on_error = OnError::Skip;
    let mut failsafe = JSValue::UNDEFINED;
    if !opts_val.is_undefined() {
        let oe = opts_val.get_property_str(cx, "onError")?;
        if !oe.is_undefined() {
            let s = oe.to_string(cx)?.content().encode_utf8(false).to_string();
            on_error = match s.as_str() {
                "throw" => OnError::Throw,
                "replace" => OnError::Replace,
                _ => OnError::Skip,
            };
        }
        failsafe = opts_val.get_property_str(cx, "failsafe")?;
    }
    Ok((on_error, failsafe))
}

/// Parse JSONL text into values.
/// Returns (values, attempted non-empty lines). `attempted == 0` means no input.
fn parse_jsonl(
    cx: &mut ScopedContext,
    text: &str,
    on_error: OnError,
    failsafe_json: Option<JsonValue>,
) -> Result<(Vec<JSValue>, usize), i32> {
    let mut out = Vec::new();
    let mut attempted = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        attempted += 1;
        match serde_json::from_str::<JsonValue>(t) {
            Ok(v) => out.push(JSValue::from_serde_json(cx, &v)?),
            Err(e) => match on_error {
                OnError::Skip => {}
                OnError::Replace => {
                    let f = failsafe_json.clone().unwrap_or(JsonValue::Null);
                    out.push(JSValue::from_serde_json(cx, &f)?);
                }
                OnError::Throw => {
                    cx.throw_syntax_error(&format!("JSONL parse error: {e}"));
                    return Err(-1);
                }
            },
        }
    }
    Ok((out, attempted))
}

/// If input has only malformed lines (attempted>0 but nothing parsed),
/// `parse`/`read` must throw.
fn check_fully_invalid(cx: &mut ScopedContext, attempted: usize, got: usize) -> Result<(), i32> {
    if attempted > 0 && got == 0 {
        cx.throw_syntax_error("JSONL.parse: no valid JSON line found");
        Err(-1)
    } else {
        Ok(())
    }
}

/// Serialize any JSON value (JSONLArray or array) to a JSONL string.
fn jsonl_stringify(cx: &mut ScopedContext, data: JSValue) -> Result<String, i32> {
    if let Some(arr) = data.as_native_object::<JSJSONLArray>() {
        return jsonl_array_stringify(cx, arr);
    }
    let json = data.to_serde_json(cx)?;
    let elems: Vec<&JsonValue> = match &json {
        JsonValue::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut out = String::new();
    for e in elems {
        let line = serde_json::to_string(e).map_err(|err| {
            cx.throw_internal_error(&err.to_string());
            -1
        })?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

// ── Module functions ─────────────────────────────────────────────────────────

/// JSONL.parse(text, opts?) — parse full JSONL into a JSONLArray.
pub(crate) fn js_parse(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text_val = cc.arg(0).to_string(cx)?;
    let text = text_val.content().encode_utf8(false).to_string();
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };
    let (values, attempted) = parse_jsonl(cx, &text, on_error, failsafe_json)?;
    check_fully_invalid(cx, attempted, values.len())?;
    make_jsonl_array(cx, values)
}

/// JSONL.tryParse(text, opts?) — never throws.
pub(crate) fn js_try_parse(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text_val = cc.arg(0).to_string(cx)?;
    let text = text_val.content().encode_utf8(false).to_string();
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let on_error = match on_error {
        OnError::Throw => OnError::Skip,
        other => other,
    };
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };
    let (values, _attempted) = parse_jsonl(cx, &text, on_error, failsafe_json)?;
    make_jsonl_array(cx, values)
}

/// JSONL.fromArray(arr) — wrap a plain JS array into a JSONLArray (elements copied).
pub(crate) fn js_from_array(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let arg = cc.arg(0);
    let mut values: Vec<JSValue> = Vec::new();
    if let Some(obj) = arg.as_object() {
        if let Ok(len) = obj.get_property_str(cx, "length")?.to_number(cx) {
            let n = len.to_i32();
            for i in 0..n {
                values.push(obj.get_property_str(cx, &i.to_string())?);
            }
        }
    }
    make_jsonl_array(cx, values)
}

/// JSONL.readFromFile(path, opts?) — read + parse a file, returns Promise<JSONLArray>.
pub(crate) fn js_read_from_file(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path_val = cc.arg(0).to_string(cx)?;
    let path = path_val.content().encode_utf8(false).to_string();
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };

    let (promise, tx) = Promise::create_with_channel::<Result<String, String>>(
        cx,
        move |cx2, payload| -> JSResult {
            let text = payload.map_err(|e| {
                cx2.throw_internal_error(&e);
                -1
            })?;
            let values = parse_jsonl(cx2, &text, on_error, failsafe_json.clone())?;
            let (values, attempted) = values;
            check_fully_invalid(cx2, attempted, values.len())?;
            make_jsonl_array(cx2, values)
        },
    )?;

    tokio::spawn(async move {
        let res = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string());
        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}

/// JSONL.countLinesFromFile(path) — estimate the number of JSONL lines in a file; returns Promise<number>.
///
/// Lightweight: only trims each line and skips blanks and the literal `undefined`.
/// It does NOT validate JSON, so the returned value is an ESTIMATE — the count from
/// `readFromFile` may be fewer (some non-blank lines can still fail to parse as JSON).
pub(crate) fn js_count_lines_from_file(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path_val = cc.arg(0).to_string(cx)?;
    let path = path_val.content().encode_utf8(false).to_string();

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(cx, move |_, data| -> JSResult {
            // Read errors resolve to 0 (matches ravenclaw countLines on missing file).
            // Cheap estimate: skip blank lines and the literal `undefined` only.
            // Intentionally avoid JSON parsing here to keep the check lightweight.
            let count = match data {
                Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).count(),
                Err(_) => 0,
            };
            Ok(JSValue::from(count as i32))
        })?;

    tokio::spawn(async move {
        let res = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string());
        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}

pub(crate) const JSONL_WRITE_APPEND: i32 = 0;
pub(crate) const JSONL_WRITE_REWRITE: i32 = 1;

/// Backend for both `appendToFile` and `rewriteToFile` (both register this fn
/// with a different `FieldDef` flag). Serializes every value to a JSONL line up
/// front (so no JS context is needed inside the async task), then writes them —
/// appending or truncating based on `cc.magic()`.
pub(crate) fn js_write_to_file(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let rewrite = cc.magic() == JSONL_WRITE_REWRITE;
    let path_val = cc.arg(0).to_string(cx)?;
    let path = path_val.content().encode_utf8(false).to_string();

    let mut lines: Vec<String> = Vec::new();
    for i in 1..cc.length() {
        let a = cc.arg(i);
        let json = a.to_serde_json(cx)?;
        let line = serde_json::to_string(&json).map_err(|e| {
            cx.throw_internal_error(&e.to_string());
            -1
        })?;
        lines.push(line);
    }
    let content = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };

    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(cx, |cx2, payload| -> JSResult {
            payload.map_err(|e| {
                cx2.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let res = async {
            use tokio::io::AsyncWriteExt;
            if rewrite {
                // Truncate (or create) the file, then write — full rewrite.
                let mut f = tokio::fs::File::create(&path).await?;
                if !content.is_empty() {
                    f.write_all(content.as_bytes()).await?;
                }
            } else {
                let mut f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .write(true)
                    .open(&path)
                    .await?;
                if !content.is_empty() {
                    f.write_all(content.as_bytes()).await?;
                }
            }
            Ok::<(), std::io::Error>(())
        }
        .await
        .map_err(|e| e.to_string());

        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}

/// JSONL.stringify(data) — serialize JSONLArray or array to JSONL string.
pub(crate) fn js_stringify(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let s = jsonl_stringify(cx, cc.arg(0))?;
    cx.create_string_from_utf8(s.into_bytes())
        .map(JSValue::from)
}

/// Parse complete lines up to the last '\n'. Returns (values, read, done).
fn parse_chunk_text(
    cx: &mut ScopedContext,
    text: &str,
    on_error: OnError,
    failsafe_json: Option<JsonValue>,
) -> Result<(Vec<JSValue>, usize, bool), i32> {
    let last_nl = text.rfind('\n');
    let complete = match last_nl {
        Some(pos) => &text[..=pos],
        None => "",
    };
    let mut out = Vec::new();
    for line in complete.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match serde_json::from_str::<JsonValue>(t) {
            Ok(v) => out.push(JSValue::from_serde_json(cx, &v)?),
            Err(_) => match on_error {
                OnError::Skip => {}
                OnError::Replace => {
                    let f = failsafe_json.clone().unwrap_or(JsonValue::Null);
                    out.push(JSValue::from_serde_json(cx, &f)?);
                }
                OnError::Throw => {
                    cx.throw_syntax_error("JSONL parse error in chunk");
                    return Err(-1);
                }
            },
        }
    }
    let (read, done) = match last_nl {
        Some(pos) => (pos + 1, text.len() - (pos + 1) == 0),
        None => (0, text.trim().is_empty()),
    };
    Ok((out, read, done))
}

/// JSONL.parseChunk(input, opts?) — incremental parse; never throws.
pub(crate) fn js_parse_chunk(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let input = cc.arg(0);
    let text = if input.is_string() {
        input
            .to_string(cx)?
            .content()
            .encode_utf8(false)
            .to_string()
    } else if let Some(obj) = input.as_object() {
        if let Ok(ta) = TypedArray::try_from(*obj) {
            String::from_utf8_lossy(ta.bytes()).to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };

    let (values, read, done) = parse_chunk_text(cx, &text, on_error, failsafe_json)?;
    let arr = make_jsonl_array(cx, values)?;

    let mut obj = cx.create_base_object()?;
    obj.define_property_value_str(cx, "values", arr, JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(cx, "read", JSValue::from(read as i32), JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(cx, "done", JSValue::Bool(done), JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(cx, "error", JSValue::NULL, JS_PROP_ENUMERABLE)?;
    Ok(obj.into())
}

thread_local! {
    static STREAM_REGISTRY: RefCell<HashMap<i32, Rc<RefCell<StreamState>>>> =
        RefCell::new(HashMap::new());
}

static STREAM_NEXT_ID: AtomicI32 = AtomicI32::new(0);

struct StreamState {
    input_iter: JSValue,
    next_pending: bool,
    finished: bool,
    buffer: String,
    queue: VecDeque<JsonValue>,
    on_error: OnError,
    failsafe: JSValue,
    id: i32,
}

struct StreamFetcher {
    state: Rc<RefCell<StreamState>>,
}

impl AsyncIteratorFetcher for StreamFetcher {
    fn try_fetch(&self, cx: &mut ScopedContext) -> Result<IterFetchStatus, i32> {
        let mut st = self.state.borrow_mut();
        if !st.queue.is_empty() {
            let json = st.queue.pop_front().unwrap();
            return Ok(IterFetchStatus::Value(JSValue::from_serde_json(cx, &json)?));
        }

        if st.finished {
            // Natural completion: drop the registry entry (return() paths do not call close).
            let id = st.id;
            drop(st);
            STREAM_REGISTRY.with(|r| r.borrow_mut().remove(&id));
            return Ok(IterFetchStatus::Done);
        }

        if !st.next_pending {
            let iter = st.input_iter;
            let next_fn = iter.get_property_str(cx, "next")?.into_callable(cx)?;
            let promise = next_fn.call(cx, iter, &[])?;
            let fulfill = cx
                .create_native_function_with_data(
                    zuqe::Callback::Func(stream_on_chunk),
                    "",
                    1,
                    0,
                    Box::new([JSValue::Int(st.id)]),
                )?
                .into();
            let reject = cx
                .create_native_function_with_data(
                    zuqe::Callback::Func(stream_on_error),
                    "",
                    1,
                    0,
                    Box::new([JSValue::Int(st.id)]),
                )?
                .into();
            let then_fn = promise.get_property_str(cx, "then")?.into_callable(cx)?;
            then_fn.call(cx, promise, &[fulfill, reject])?;
            st.next_pending = true;
        }
        Ok(IterFetchStatus::Wait)
    }

    fn close(&self) {
        let id = self.state.borrow().id;
        STREAM_REGISTRY.with(|r| r.borrow_mut().remove(&id));
    }
}

/// Parse complete lines (up to last '\n') from the buffer into the queue.
fn feed_buffer(cx: &mut ScopedContext, st: &mut StreamState) -> Result<(), i32> {
    if let Some(pos) = st.buffer.rfind('\n') {
        let complete = &st.buffer[..=pos];
        for line in complete.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            match serde_json::from_str::<JsonValue>(t) {
                Ok(v) => st.queue.push_back(v),
                Err(_) => match st.on_error {
                    OnError::Skip => {}
                    OnError::Replace => {
                        let f = if st.failsafe.is_undefined() {
                            JsonValue::Null
                        } else {
                            st.failsafe.to_serde_json(cx)?
                        };
                        st.queue.push_back(f);
                    }
                    OnError::Throw => {
                        cx.throw_syntax_error("JSONL parse error in stream");
                        return Err(-1);
                    }
                },
            }
        }
        st.buffer.drain(..=pos);
    }
    Ok(())
}

fn chunk_to_string(cx: &mut ScopedContext, v: JSValue) -> String {
    if v.is_string() {
        v.to_string(cx)
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default()
    } else if let Some(obj) = v.as_object() {
        if let Ok(ta) = TypedArray::try_from(*obj) {
            String::from_utf8_lossy(ta.bytes()).to_string()
        } else {
            String::new()
        }
    } else {
        String::new()
    }
}

fn stream_on_chunk(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id = match cc.data(0) {
        JSValue::Int(i) => i,
        _ => return Ok(JSValue::UNDEFINED),
    };
    let res = cc.arg(0);
    let done = res
        .get_property_str(cx, "done")
        .and_then(|v| v.to_bool())
        .unwrap_or(false);
    if let Some(st) = STREAM_REGISTRY.with(|r| r.borrow().get(&id).cloned()) {
        let mut g = st.borrow_mut();
        if done {
            g.finished = true;
        } else {
            let v = res.get_property_str(cx, "value")?;
            let s = chunk_to_string(cx, v);
            g.buffer.push_str(&s);
            feed_buffer(cx, &mut g)?;
        }
        g.next_pending = false;
    }
    Ok(JSValue::UNDEFINED)
}

fn stream_on_error(_cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id = match cc.data(0) {
        JSValue::Int(i) => i,
        _ => return Ok(JSValue::UNDEFINED),
    };
    if let Some(st) = STREAM_REGISTRY.with(|r| r.borrow().get(&id).cloned()) {
        let mut g = st.borrow_mut();
        g.finished = true;
        g.next_pending = false;
    }
    Ok(JSValue::UNDEFINED)
}

/// Get the async iterator of a stream (aike's fetch body / FileHandle expose `next`).
fn get_async_iterator(cx: &mut ScopedContext, stream: JSValue) -> JSResult {
    let next_prop = stream.get_property_str(cx, "next")?;
    if !next_prop.is_undefined() && next_prop.is_callable() {
        return Ok(stream);
    }
    cx.throw_type_error("JSONL.parseStream: argument is not an async iterable (no `next` method)");
    Err(-1)
}

/// JSONL.parseStream(stream, opts?) — async iterable of parsed values.
pub(crate) fn js_parse_stream(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let stream = cc.arg(0);
    let iter = get_async_iterator(cx, stream)?;
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let on_error = match on_error {
        OnError::Throw => OnError::Skip,
        other => other,
    };

    let state = Rc::new(RefCell::new(StreamState {
        input_iter: iter,
        next_pending: false,
        finished: false,
        buffer: String::new(),
        queue: VecDeque::new(),
        on_error,
        failsafe,
        id: 0,
    }));
    let id = STREAM_NEXT_ID.fetch_add(1, Ordering::Relaxed) + 1;
    STREAM_REGISTRY.with(|r| {
        r.borrow_mut().insert(id, state.clone());
    });
    state.borrow_mut().id = id;

    let fetcher = StreamFetcher { state };
    let iter_obj = AsyncIterableIterator::create(cx, fetcher)?;
    // input_iter / failsafe live in the fetcher (not GC-traced), so pin them on
    // the iterator object to avoid dangling references after collection.
    let mut iter_val: JSValue = iter_obj.into();
    iter_val.define_property_value_str(cx, "__src", iter, JS_PROP_C_W_E)?;
    iter_val.define_property_value_str(cx, "__failsafe", failsafe, JS_PROP_C_W_E)?;
    Ok(iter_val)
}
