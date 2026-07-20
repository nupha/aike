use std::path::PathBuf;

use serde_json::Value as JsonValue;
use zuqe::{
    Array, CallContext, JSResult, JSValue, PollSender, ScopedContext,
    builtins::{iterator::AsyncIterableIterator, promise::Promise},
};

// magic const: 0 = append, 1 = rewrite/truncate
pub(crate) const JSONL_WRITE_APPEND: i32 = 0;
pub(crate) const JSONL_WRITE_REWRITE: i32 = 1;

#[derive(Clone, Copy, PartialEq)]
enum OnError {
    Replace,
    Skip,
    Throw,
}

// "throw" degrades to "skip": parse never throws.
fn on_error_from_str(s: &str) -> OnError {
    match s {
        "replace" => OnError::Replace,
        "skip" => OnError::Skip,
        "throw" => OnError::Throw,
        _ => OnError::Replace,
    }
}

fn extract_opts(cx: &mut ScopedContext, arg: JSValue) -> Result<(OnError, JSValue), i32> {
    if arg.is_undefined() {
        return Ok((OnError::Replace, JSValue::UNDEFINED));
    }
    let obj = match arg.to_object() {
        Some(o) => o,
        None => return Ok((OnError::Replace, JSValue::UNDEFINED)),
    };
    let on_error = match obj.get_property_str(cx, "onError") {
        Ok(v) if !v.is_undefined() => match v.to_string(cx) {
            Ok(s) => on_error_from_str(&s.content().encode_utf8(false).to_string()),
            Err(_) => OnError::Replace,
        },
        _ => OnError::Replace,
    };
    let failsafe = match obj.get_property_str(cx, "failsafe") {
        Ok(v) => v,
        Err(_) => JSValue::UNDEFINED,
    };
    Ok((on_error, failsafe))
}

// Each line: trim, skip blank, parse, tolerate bad line per onError.
fn parse_jsonl(
    cx: &mut ScopedContext,
    text: &str,
    on_error: OnError,
    failsafe_json: Option<JsonValue>,
) -> Result<(Vec<JSValue>, usize), i32> {
    let mut values: Vec<JSValue> = Vec::new();
    let mut attempted: usize = 0;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        attempted += 1;
        let parsed: JsonValue = match serde_json::from_str::<JsonValue>(trimmed) {
            Ok(v) => v,
            Err(_) => match on_error {
                OnError::Replace => failsafe_json.clone().unwrap_or(JsonValue::Null),
                OnError::Skip => continue,
                OnError::Throw => continue,
            },
        };
        match JSValue::from_serde_json(cx, &parsed) {
            Ok(v) => values.push(v),
            Err(code) => return Err(code),
        }
    }
    Ok((values, attempted))
}

// One JSON value -> one compact JSON line.
fn jsonl_stringify(cx: &mut ScopedContext, arg: JSValue) -> Result<String, i32> {
    let json = arg.to_serde_json(cx)?;
    let mut s = serde_json::to_string(&json).map_err(|e| {
        cx.throw_syntax_error(&format!("JSONL stringify failed: {}", e));
        -1
    })?;
    s.push('\n');
    Ok(s)
}

// JSONL.parse(text, opts?) -> T[]
pub(crate) fn js_parse(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text = cc.arg(0).to_string(cx)?;
    let content = text.content().encode_utf8(false).to_string();
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };
    let (values, _attempted) =
        parse_jsonl(cx, &content, on_error, failsafe_json).map_err(|code| code)?;
    Array::from_values(cx, values).map(JSValue::from)
}

// JSONL.stringify(data) -> string
pub(crate) fn js_stringify(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let arg = cc.arg(0);
    let json = arg.to_serde_json(cx)?;
    let out = match &json {
        JsonValue::Array(arr) => {
            let mut s = String::new();
            for item in arr {
                s.push_str(&serde_json::to_string(item).map_err(|e| {
                    cx.throw_syntax_error(&format!("JSONL stringify failed: {}", e));
                    -1
                })?);
                s.push('\n');
            }
            s
        }
        other => {
            serde_json::to_string(other).map_err(|e| {
                cx.throw_syntax_error(&format!("JSONL stringify failed: {}", e));
                -1
            })? + "\n"
        }
    };
    // out is already a valid JSON string; wrap and convert to JS string.
    let json_str = JsonValue::String(out);
    JSValue::from_serde_json(cx, &json_str)
}

// path pre-resolved (caller decides sandbox). Reads file -> T[].
fn read_jsonl_impl(
    cx: &mut ScopedContext,
    path: PathBuf,
    on_error: OnError,
    failsafe_json: Option<JsonValue>,
) -> JSResult {
    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(cx, move |cx2, data| -> JSResult {
            let text = match data {
                Ok(t) => t,
                Err(e) => {
                    cx2.throw_internal_error(&e);
                    return Err(-1);
                }
            };
            let (values, _attempted) =
                parse_jsonl(cx2, &text, on_error, failsafe_json).map_err(|code| code)?;
            Array::from_values(cx2, values).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let res = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string());
        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}

// JSONL.read(path, opts?) — no sandbox.
pub(crate) fn js_read_jsonl(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path_val = cc.arg(0).to_string(cx)?;
    let path = PathBuf::from(path_val.content().encode_utf8(false).to_string());
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };
    read_jsonl_impl(cx, path, on_error, failsafe_json)
}

// fs.readJSONL — caller already applied sandbox to `path`.
pub(crate) fn read_jsonl_with_path(
    cx: &mut ScopedContext,
    cc: CallContext,
    path: PathBuf,
) -> JSResult {
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };
    read_jsonl_impl(cx, path, on_error, failsafe_json)
}

fn build_jsonl_lines(cx: &mut ScopedContext, cc: CallContext) -> Result<Vec<String>, i32> {
    let mut lines: Vec<String> = Vec::new();
    let argc = cc.length();
    for i in 1..argc {
        let arg = cc.arg(i);
        let json = jsonl_stringify(cx, arg)?;
        lines.push(json);
    }
    Ok(lines)
}

// path pre-resolved. append=true appends, else truncates.
fn write_jsonl_impl(
    cx: &mut ScopedContext,
    path: PathBuf,
    append: bool,
    lines: Vec<String>,
) -> JSResult {
    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(cx, |_cx, data| -> JSResult {
            data.map_err(|e| {
                _cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let res = async {
            if append {
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await?;
                for line in &lines {
                    use tokio::io::AsyncWriteExt;
                    file.write_all(line.as_bytes()).await?;
                }
            } else {
                let mut content = String::new();
                for line in &lines {
                    content.push_str(line);
                }
                // atomic replace, so readers never see a torn jsonl.
                crate::fs::write_file_atomic(&path, content.as_bytes()).await?;
            }
            Ok::<(), std::io::Error>(())
        }
        .await
        .map_err(|e| e.to_string());
        let _ = PollSender::from(tx).send_async(res).await;
    });

    Ok(promise.into())
}

// JSONL.append / JSONL.write — magic selects mode. No sandbox.
pub(crate) fn js_write_jsonl(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let path_val = cc.arg(0).to_string(cx)?;
    let path = PathBuf::from(path_val.content().encode_utf8(false).to_string());
    let append = cc.magic() == JSONL_WRITE_APPEND;
    let lines = build_jsonl_lines(cx, cc)?;
    write_jsonl_impl(cx, path, append, lines)
}

// fs.appendJSONL / fs.writeJSONL — caller already applied sandbox to `path`.
pub(crate) fn write_jsonl_with_path(
    cx: &mut ScopedContext,
    cc: CallContext,
    path: PathBuf,
) -> JSResult {
    let append = cc.magic() == JSONL_WRITE_APPEND;
    let lines = build_jsonl_lines(cx, cc)?;
    write_jsonl_impl(cx, path, append, lines)
}

// onError "throw" kept (unlike parse): bad line rejects the async iterator.
pub(crate) fn js_parse_chunk(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text = cc.arg(0).to_string(cx)?;
    let content = text.content().encode_utf8(false).to_string();
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };
    let (values, attempted) =
        parse_jsonl(cx, &content, on_error, failsafe_json).map_err(|code| code)?;
    let mut obj = cx.create_base_object()?;
    let arr = Array::from_values(cx, values)?;
    obj.define_property_value_str(cx, "values", arr.into(), 0)?;
    obj.define_property_value_str(cx, "attempted", JSValue::from(attempted as f64), 0)?;
    Ok(obj.into())
}

// JSONL.parseStream(text, opts?) -> { iterator: AsyncIterable<T>, lines: number }.
// The producer runs on a background thread: create_with_channel uses a bounded
// channel (cap 1), so sending must NOT happen synchronously inside this call —
// it would deadlock, because the consumer (for await) only runs later on the JS
// event loop. With the producer on another thread, each pulled value frees a
// slot for the next send.
pub(crate) fn js_parse_stream(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text = cc.arg(0).to_string(cx)?;
    let text_str = text.content().encode_utf8(false).to_string();
    let (on_error, failsafe) = extract_opts(cx, cc.arg(1))?;
    let failsafe_json = if failsafe.is_undefined() {
        None
    } else {
        Some(failsafe.to_serde_json(cx)?)
    };

    // Non-blank line count, matching parse semantics (cheap up-front scan).
    let lines = text_str.lines().filter(|l| !l.trim().is_empty()).count();

    let (iter, tx) = AsyncIterableIterator::create_with_channel::<Result<JsonValue, String>>(
        cx,
        move |ctx2, data| -> Result<Option<JSValue>, i32> {
            let v = data.map_err(|e| {
                ctx2.throw_internal_error(&e);
                -1
            })?;
            Ok(Some(JSValue::from_serde_json(ctx2, &v)?))
        },
    )?;

    // Produce on a background thread so the bounded channel never blocks the JS
    // call. Consumer (for await) pulls one value per tick, freeing a slot.
    std::thread::spawn(move || {
        for line in text_str.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: JsonValue = match serde_json::from_str::<JsonValue>(trimmed) {
                Ok(v) => v,
                Err(e) => match on_error {
                    OnError::Replace => failsafe_json.clone().unwrap_or(JsonValue::Null),
                    OnError::Skip => continue,
                    OnError::Throw => {
                        let _ = tx.send(Err(e.to_string()));
                        return;
                    }
                },
            };
            if tx.send(Ok(parsed)).is_err() {
                return; // consumer dropped; nothing more to send
            }
        }
    });

    let mut obj = cx.create_base_object()?;
    let iter_val: JSValue = iter.into();
    obj.define_property_value_str(cx, "iterator", iter_val, 0)?;
    obj.define_property_value_str(cx, "lines", JSValue::from(lines as f64), 0)?;
    Ok(obj.into())
}
