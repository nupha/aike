// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, time::Duration};

use crossbeam::channel;
use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassID, FieldDef, JSResult, JSValue, JobState, NativeObject, Runtime,
    ScopedContext,
    builtins::{
        iterator::AsyncIterableIterator,
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
    def_native_object,
};

use crossbeam::channel::TryRecvError;

use crate::USER_CLASS_DEFS;

struct ResponsePayload {
    resp: Option<reqwest::Response>,
    body_rx: Option<crossbeam::channel::Receiver<Vec<u8>>>,
}

impl ResponsePayload {
    fn take_body_rx(&mut self) -> Option<crossbeam::channel::Receiver<Vec<u8>>> {
        self.body_rx.take()
    }

    /// Ensure body streaming has started. Called lazily on first body access.
    fn ensure_body_stream(&mut self) -> Result<(), i32> {
        if self.body_rx.is_some() {
            return Ok(()); // already streaming
        }

        if let Some(resp) = self.resp.take() {
            let (body_tx, body_rx) = channel::unbounded::<Vec<u8>>();
            self.body_rx = Some(body_rx);

            tokio::spawn(async move {
                use futures_util::StreamExt;

                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if body_tx.send(bytes.to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            body_tx.send(format!("{e}").into_bytes()).ok();
                            break;
                        }
                    }
                }
            });
            Ok(())
        } else {
            Err(-1) // body already consumed or request never succeeded
        }
    }
}

/// Get a `&mut ResponseData` from a Response instance's opaque pointer.
fn get_response_data<'a>(
    ctx: &'a mut ScopedContext,
    val: &JSValue,
) -> Result<&'a mut ResponsePayload, i32> {
    let payload = val
        .retrive_object_payload(ctx, JSResponse::class_id())?
        .other()
        .unwrap();
    let opaque = payload.opaque();
    if opaque.is_null() {
        ctx.throw_type_error("Response was detached");
        Err(-1)
    } else {
        Ok(unsafe { &mut *opaque.cast::<ResponsePayload>() })
    }
}

struct ResponseMeta {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    content_type: String,
    content_length: Option<u64>,
}

/// request(url, options?)  –  returns Promise<HttpResponse>
pub(crate) fn js_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let url_str = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    // parse options
    let mut method = String::from("GET");
    let mut req_headers: Vec<(String, String)> = Vec::new();
    let mut req_body: Option<String> = None;
    let mut timeout = Duration::from_secs(30);

    if cc.length() > 1 {
        let arg1 = cc.arg(1);
        if let Some(opts) = arg1.as_object() {
            // method
            let v = opts.get_property_str(ctx, "method")?;
            if !v.is_undefined() {
                method = v
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string()
                    .to_uppercase();
            }

            // headers
            let mut v = opts.get_property_str(ctx, "headers")?;
            if let Some(hdr_obj) = v.as_object_mut() {
                let pks = hdr_obj.get_own_property_names(
                    ctx,
                    zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_INDEX_MASK,
                )?;
                for pk in pks.iter() {
                    let key_v = pk.atom.to_value(ctx)?;
                    let key_r = key_v.to_string(ctx)?;
                    let key_str = key_r.content().encode_utf8(false).to_string();
                    let val = hdr_obj.get_property(ctx, pk.atom)?;
                    req_headers.push((
                        key_str,
                        val.to_string(ctx)?.content().encode_utf8(false).to_string(),
                    ));
                }
            }

            // body
            let v = opts.get_property_str(ctx, "body")?;
            if !v.is_undefined_or_null() {
                let s = v.to_string(ctx)?;
                req_body = Some(s.content().encode_utf8(false).to_string());
            }

            // timeout (secs) — None = no timeout per reqwest default
            let timeout_s = opts.get_property_str(ctx, "timeout")?;
            if !timeout_s.is_undefined() {
                let n = timeout_s.to_number(ctx)?.to_i32();
                if n > 0 {
                    timeout = Duration::from_secs(n as u64);
                }
            }
        }
    }

    let resp_holder = std::sync::Arc::new(std::sync::Mutex::new(None::<reqwest::Response>));
    let resp_holder2 = resp_holder.clone();

    //
    // Promise that resolves when reqwest headers arrive.
    // The async task builds ResponseMeta and stores the reqwest::Response;
    // the callback builds the Response class instance without body streaming.
    // Body streaming is triggered lazily on .text()/.json()/.body access.
    //
    let result_slot = PromiseResultSlot::<Result<ResponseMeta, String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, payload| -> JSResult {
            let meta = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;

            // Take the stored response (may be None if close() was called early)
            let resp = resp_holder.lock().unwrap().take();

            let mut resp_obj = JSResponse::new_instance(
                cx,
                JSValue::UNDEFINED,
                ResponsePayload {
                    body_rx: None,
                    resp,
                },
            )?;

            resp_obj.define_property_value_str(
                cx,
                "status",
                JSValue::from(meta.status as i32),
                zuqe::JS_PROP_C_W_E,
            )?;
            resp_obj.define_property_value_str(
                cx,
                "ok",
                JSValue::Bool(meta.status >= 200 && meta.status < 300),
                zuqe::JS_PROP_C_W_E,
            )?;
            {
                let s = cx.create_string_from_utf8(meta.status_text.into_bytes())?;
                resp_obj.define_property_value_str(
                    cx,
                    "statusText",
                    s.into(),
                    zuqe::JS_PROP_C_W_E,
                )?;
            }

            {
                let s = cx.create_string_from_utf8(meta.content_type.into_bytes())?;
                resp_obj.define_property_value_str(
                    cx,
                    "contentType",
                    s.into(),
                    zuqe::JS_PROP_C_W_E,
                )?;
            }

            resp_obj.define_property_value_str(
                cx,
                "contentLength",
                meta.content_length
                    .map(|v| JSValue::from(v as f64))
                    .unwrap_or(JSValue::NULL),
                zuqe::JS_PROP_C_W_E,
            )?;

            // headers (plain JS object)
            {
                let mut hdr_obj = cx.create_base_object()?;
                for (k, v) in &meta.headers {
                    let val = cx.create_string_from_utf8(v.as_bytes().to_vec())?;
                    hdr_obj.define_property_value_str(cx, k, val.into(), zuqe::JS_PROP_C_W_E)?;
                }
                resp_obj.define_property_value_str(
                    cx,
                    "headers",
                    hdr_obj.into(),
                    zuqe::JS_PROP_C_W_E,
                )?;
            }

            Ok(resp_obj.into()) // Promise resolve with the Response object
        },
    )?;

    //
    // spawn HTTP reqwest async task
    //
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut req = client
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET),
                &url_str,
            )
            .timeout(timeout);

        for (k, v) in &req_headers {
            req = req.header(k, v);
        }
        if let Some(b) = &req_body {
            req = req.body(b.clone());
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let content_type = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let content_length = resp.content_length();

                let hdrs: Vec<(String, String)> = resp
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| v.to_str().ok().map(|vs| (k.to_string(), vs.to_string())))
                    .collect();

                let meta = ResponseMeta {
                    status: status.as_u16(),
                    status_text: status.canonical_reason().unwrap_or("").to_string(),
                    headers: hdrs,
                    content_type,
                    content_length,
                };

                if result_slot.clone().commit(Ok(meta)).is_err() {
                    return;
                }

                // Store the response for lazy body streaming.
                // Body will be streamed on first .text()/.json()/.body access.
                *resp_holder2.lock().unwrap() = Some(resp);
            }
            Err(e) => {
                result_slot.commit(Err(format!("{e}"))).ok();
            }
        }
    });

    Ok(promise.into())
}

/// Lazily create body AsyncIterableIterator from the stored receiver.
/// On first access the receiver is taken; subsequent calls throw.
fn js_response_body(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    // Step 1: ensure body streaming started (releases ctx borrow before step 2)
    let has_body = {
        let data = get_response_data(ctx, &cc.this())?;
        data.ensure_body_stream().is_ok()
    };
    if !has_body {
        ctx.throw_type_error("body already consumed");
        return Err(-1);
    }

    let rx = {
        let data = get_response_data(ctx, &cc.this())?;
        data.take_body_rx().unwrap()
    };

    let iter = AsyncIterableIterator::create_with_receiver::<Vec<u8>>(
        ctx,
        rx,
        |cx2, chunk| -> Result<Option<JSValue>, i32> {
            TypedArray::create_from_vec_u8(cx2, chunk)
                .map(JSValue::from)
                .map(Some)
        },
    )
    .map_err(|_| {
        ctx.throw_internal_error("failed to create body iterator");
        -1
    })?;

    Ok(iter.into())
}

const BODY_AS_BYTES: i32 = 0;
const BODY_AS_TEXT: i32 = 1;
const BODY_AS_JSON: i32 = 2;

/// Strip HTML tags, decode common entities, and remove script/style content.
/// Handles multi-byte UTF-8 correctly — iterates by char, matches by byte offset.
fn strip_html_tags(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let mut result = String::with_capacity(input.len() / 2);
    let mut in_tag = false;
    let mut in_skip = false; // inside <script> or <style>

    // Collect (byte_offset, char) for UTF-8-safe iteration
    let char_pos: Vec<(usize, char)> = input.char_indices().collect();
    let mut ci = 0;

    while ci < char_pos.len() {
        let (pos, ch) = char_pos[ci];

        if !in_tag && ch == '<' {
            // Detect <script or <style opening (all ASCII, safe to slice lower)
            if !in_skip {
                let rest = &lower[pos..];
                if rest.starts_with("<script")
                    && rest.len() > 7
                    && (lower_bytes[pos + 7] == b'>' || lower_bytes[pos + 7].is_ascii_whitespace())
                {
                    in_skip = true;
                } else if rest.starts_with("<style")
                    && rest.len() > 6
                    && (lower_bytes[pos + 6] == b'>' || lower_bytes[pos + 6].is_ascii_whitespace())
                {
                    in_skip = true;
                }
            }
            in_tag = true;
            ci += 1;
            continue;
        }
        if in_tag && ch == '>' {
            in_tag = false;
            // Detect </script> or </style> closing
            if in_skip && pos >= 8 {
                let from = pos.saturating_sub(10);
                let slice = &lower[from..=pos];
                if slice.contains("</script>") || slice.contains("</style>") {
                    in_skip = false;
                }
            }
            result.push(' ');
            ci += 1;
            continue;
        }
        if !in_tag && !in_skip {
            if ch == '&' {
                // Decode common HTML entities (all ASCII)
                let rest = &lower[pos..];
                let (decoded, skip) = if rest.starts_with("&amp;") {
                    ('&', 5)
                } else if rest.starts_with("&lt;") {
                    ('<', 4)
                } else if rest.starts_with("&gt;") {
                    ('>', 4)
                } else if rest.starts_with("&quot;") {
                    ('"', 6)
                } else if rest.starts_with("&#39;") {
                    ('\'', 5)
                } else if rest.starts_with("&apos;") {
                    ('\'', 6)
                } else if rest.starts_with("&nbsp;") {
                    (' ', 6)
                } else {
                    ('&', 1)
                };
                result.push(decoded);
                // Advance past multi-byte entity by byte offset
                let target = pos + skip;
                while ci < char_pos.len() && char_pos[ci].0 < target {
                    ci += 1;
                }
            } else {
                // Push the full UTF-8 char (safe for CJK, emoji, etc.)
                result.push(ch);
                ci += 1;
            }
        } else {
            ci += 1;
        }
    }

    // Collapse whitespace
    let mut collapsed = String::with_capacity(result.len());
    let mut last_was_space = false;
    for ch in result.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                collapsed.push(' ');
                last_was_space = true;
            }
        } else {
            collapsed.push(ch);
            last_was_space = false;
        }
    }
    collapsed.trim().to_string()
}

/// Response.bytes(), .text(convert?), .json()  –  drain body, return Promise<T>
/// `text("strip")` strips HTML tags for clean plain text.
fn js_response_body_as(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let magic = cc.magic();

    // For text mode, check optional convert argument
    let arg0 = cc.arg(0);
    let strip_tags = magic == BODY_AS_TEXT
        && !arg0.is_undefined()
        && arg0.to_string(ctx).ok().map_or(false, |s| {
            s.content().encode_utf8(false).as_ref() == "strip"
        });

    // Step 1: ensure body streaming started (releases ctx borrow before step 2)
    let has_body = {
        let data = get_response_data(ctx, &cc.this())?;
        data.ensure_body_stream().is_ok()
    };
    if !has_body {
        ctx.throw_type_error("body already consumed");
        return Err(-1);
    }

    let rx = {
        let data = get_response_data(ctx, &cc.this())?;
        data.take_body_rx().unwrap()
    };

    let result_slot = PromiseResultSlot::<Result<Vec<u8>, String>>::new();
    let promise = Promise::create_with_result_slot(
        ctx,
        result_slot.clone(),
        move |cx, payload| -> JSResult {
            let buf = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            match magic {
                BODY_AS_BYTES => TypedArray::create_from_vec_u8(cx, buf).map(JSValue::from),
                BODY_AS_TEXT => {
                    let s = String::from_utf8_lossy(&buf);
                    let out = if strip_tags {
                        strip_html_tags(&s)
                    } else {
                        s.into_owned()
                    };
                    cx.create_string_from_utf8(out.into_bytes())
                        .map(JSValue::from)
                }
                BODY_AS_JSON => {
                    let s = String::from_utf8_lossy(&buf);
                    let val: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
                        cx.throw_internal_error(&e.to_string());
                        -1
                    })?;
                    JSValue::from_serde_json(cx, &val)
                }
                _ => unreachable!(),
            }
        },
    )?;

    // Poll body chunks via queue_job (async, non-blocking)
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Some(Vec::new())));
    let start = std::time::Instant::now();
    let body_timeout = Duration::from_secs(30);

    ctx.queue_job(
        move |_cx| {
            let mut guard = buf.lock().unwrap();
            let inner = guard.as_mut().unwrap();
            match rx.try_recv() {
                Ok(chunk) => {
                    inner.extend_from_slice(&chunk);
                    Ok(JobState::Again)
                }
                Err(TryRecvError::Disconnected) => {
                    let full = guard.take().unwrap();
                    let _ = result_slot.commit(Ok(full));
                    Ok(JobState::Done)
                }
                Err(TryRecvError::Empty) => {
                    if start.elapsed() > body_timeout {
                        guard.take();
                        let _ = result_slot.commit(Err("body read timeout".into()));
                        Ok(JobState::Done)
                    } else {
                        Ok(JobState::Again)
                    }
                }
            }
        },
        (),
    );

    Ok(promise.into())
}

/// response.close()  —  drop body rx and stored response, stop streaming task
fn js_response_close(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let data = get_response_data(ctx, &cc.this())?;
    drop(data.body_rx.take());
    drop(data.resp.take());
    Ok(JSValue::UNDEFINED)
}

const RESPONSE_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("body", Some(js_response_body), None, 0),
    FieldDef::FUNC("bytes", 0, js_response_body_as, BODY_AS_BYTES as i16),
    FieldDef::FUNC("text", 0, js_response_body_as, BODY_AS_TEXT as i16),
    FieldDef::FUNC("json", 0, js_response_body_as, BODY_AS_JSON as i16),
    FieldDef::FUNC("close", 0, js_response_close, 0),
];

def_native_object!(
    "HttpResponse",
    JSResponse,
    ResponsePayload,
    RESPONSE_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_response(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSResponse::register(rt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_basic_html() {
        let html = "<p>Hello <b>world</b></p>";
        assert_eq!(strip_html_tags(html), "Hello world");
    }

    #[test]
    fn strip_script_style() {
        let html = "<div>text</div><script>alert(1)</script><style>.a{color:red}</style><p>ok</p>";
        assert_eq!(strip_html_tags(html), "text ok");
    }

    #[test]
    fn strip_entities() {
        let html = "<p>A &amp; B &lt; C &gt; D &quot;E&quot; F&#39;s G&nbsp;H</p>";
        assert_eq!(strip_html_tags(html), "A & B < C > D \"E\" F's G H");
    }

    #[test]
    fn strip_chinese_utf8() {
        let html = "<h1>你好世界</h1><p>这是一个<strong>测试</strong>页面。</p>";
        assert_eq!(strip_html_tags(html), "你好世界 这是一个 测试 页面。");
    }

    #[test]
    fn strip_japanese_korean() {
        let html = "<p>日本語テスト</p><p>한국어 테스트</p>";
        assert_eq!(strip_html_tags(html), "日本語テスト 한국어 테스트");
    }

    #[test]
    fn strip_emoji() {
        let html = "<p>Hello 🌍🌎🌏</p><span>✅ Done</span>";
        assert_eq!(strip_html_tags(html), "Hello 🌍🌎🌏 ✅ Done");
    }

    #[test]
    fn strip_mixed_cjk_ascii() {
        let html = "<div>Price: ¥100 / €50 / $30</div><p>商品名：テスト用商品</p>";
        assert_eq!(
            strip_html_tags(html),
            "Price: ¥100 / €50 / $30 商品名：テスト用商品"
        );
    }

    #[test]
    fn strip_script_with_attrs() {
        let html = "<p>before</p><script type=\"text/javascript\">var x=1;</script><p>after</p>";
        assert_eq!(strip_html_tags(html), "before after");
    }

    #[test]
    fn strip_whitespace_collapse() {
        let html = "<p>  hello   \n  world  </p>";
        assert_eq!(strip_html_tags(html), "hello world");
    }

    #[test]
    fn strip_empty_input() {
        assert_eq!(strip_html_tags(""), "");
    }

    #[test]
    fn strip_no_tags() {
        assert_eq!(strip_html_tags("plain text"), "plain text");
    }

    #[test]
    fn strip_real_world_nav() {
        let html = "<nav><a href='/'>Home</a><a href='/about'>About</a></nav>\
                     <main><h1>标题：Rust 性能优化</h1>\
                     <p>本文介绍了 <code>char_indices()</code> 的用法。</p></main>";
        let result = strip_html_tags(html);
        assert!(result.contains("标题：Rust 性能优化"));
        assert!(result.contains("char_indices()"));
        assert!(result.contains("Home"));
    }
}
