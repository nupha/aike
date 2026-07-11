// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::time::Duration;

use zuqe::{
    CallContext, JSResult, ScopedContext,
    builtins::promise::{Promise, PromiseResultSlot},
};

use crate::whatwg::fetch::{
    headers::{HeaderList, header_list_from_js},
    response::build_response_from_transport,
    transport::{TransportResponse, perform_request, request_with_headers},
};

/// request(url, options?) — returns Promise<Response>
///
/// Delegates to the shared `fetch` transport and returns the unified
/// `Response` class (same as `fetch()`).
pub(crate) fn js_request(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let url_str = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();

    let mut method = String::from("GET");
    let mut req_headers = HeaderList::new();
    let mut req_body: Option<String> = None;
    let mut timeout = Duration::from_secs(30);

    if cc.length() > 1 {
        let arg1 = cc.arg(1);
        if let Some(opts) = arg1.as_object() {
            let v = opts.get_property_str(ctx, "method")?;
            if !v.is_undefined() {
                method = v
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string()
                    .to_uppercase();
            }

            let mut h = opts.get_property_str(ctx, "headers")?;
            if !h.is_undefined_or_null() {
                req_headers = header_list_from_js(ctx, &mut h)?;
            }

            let b = opts.get_property_str(ctx, "body")?;
            if !b.is_undefined_or_null() {
                req_body = Some(b.to_string(ctx)?.content().encode_utf8(false).to_string());
            }

            let t = opts.get_property_str(ctx, "timeout")?;
            if !t.is_undefined() {
                let n = t.to_number(ctx)?.to_i32();
                if n > 0 {
                    timeout = Duration::from_secs(n as u64);
                }
            }
        }
    }

    let result_slot = PromiseResultSlot::<Result<TransportResponse, String>>::new();
    let url_clone = url_str.clone();
    let promise =
        Promise::create_with_result_slot(ctx, result_slot.clone(), move |cx, data| -> JSResult {
            let tr = data.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            build_response_from_transport(cx, tr, "basic", &url_clone, false)
        })?;

    let req = request_with_headers(
        method,
        url_str,
        req_headers,
        req_body.map(|s| s.into_bytes()),
        timeout,
    );
    tokio::spawn(async move {
        let r = perform_request(req).await;
        let _ = result_slot.commit(r);
    });

    Ok(promise.into())
}

#[cfg(test)]
mod tests {
    use crate::html::{HtmlConvertMode, convert_html};

    #[test]
    fn strip_basic_html() {
        let html = "<p>Hello <b>world</b></p>";
        assert_eq!(convert_html(html, HtmlConvertMode::Strip), "Hello world");
    }

    #[test]
    fn strip_script_style() {
        let html = "<div>text</div><script>alert(1)</script><style>.a{color:red}</style><p>ok</p>";
        assert_eq!(convert_html(html, HtmlConvertMode::Strip), "text ok");
    }

    #[test]
    fn strip_entities() {
        let html = "<p>A &amp; B &lt; C &gt; D &quot;E&quot; F&#39;s G&nbsp;H</p>";
        assert_eq!(
            convert_html(html, HtmlConvertMode::Strip),
            "A & B < C > D \"E\" F's G H"
        );
    }

    #[test]
    fn strip_chinese_utf8() {
        let html = "<h1>你好世界</h1><p>这是一个<strong>测试</strong>页面。</p>";
        assert_eq!(
            convert_html(html, HtmlConvertMode::Strip),
            "你好世界 这是一个 测试 页面。"
        );
    }

    #[test]
    fn strip_japanese_korean() {
        let html = "<p>日本語テスト</p><p>한국어 테스트</p>";
        assert_eq!(
            convert_html(html, HtmlConvertMode::Strip),
            "日本語テスト 한국어 테스트"
        );
    }
    #[test]
    fn strip_emoji() {
        let html = "<p>Hello 🌍🌎🌏</p><span>✅ Done</span>";
        assert_eq!(
            convert_html(html, HtmlConvertMode::Strip),
            "Hello 🌍🌎🌏 ✅ Done"
        );
    }

    #[test]
    fn strip_mixed_cjk_ascii() {
        let html = "<div>Price: ¥100 / €50 / $30</div><p>商品名：テスト用商品</p>";
        assert_eq!(
            convert_html(html, HtmlConvertMode::Strip),
            "Price: ¥100 / €50 / $30 商品名：テスト用商品"
        );
    }

    #[test]
    fn strip_script_with_attrs() {
        let html = "<p>before</p><script type=\"text/javascript\">var x=1;</script><p>after</p>";
        assert_eq!(convert_html(html, HtmlConvertMode::Strip), "before after");
    }

    #[test]
    fn strip_whitespace_collapse() {
        let html = "<p>  hello   \n  world  </p>";
        assert_eq!(convert_html(html, HtmlConvertMode::Strip), "hello world");
    }

    #[test]
    fn strip_empty_input() {
        assert_eq!(convert_html("", HtmlConvertMode::Strip), "");
    }

    #[test]
    fn strip_no_tags() {
        assert_eq!(
            convert_html("plain text", HtmlConvertMode::Strip),
            "plain text"
        );
    }

    #[test]
    fn strip_real_world_nav() {
        let html = "<nav><a href='/'>Home</a><a href='/about'>About</a></nav>\
                     <main><h1>标题：Rust 性能优化</h1>\
                     <p>本文介绍了 <code>char_indices()</code> 的用法。</p></main>";
        let result = convert_html(html, HtmlConvertMode::Strip);
        assert!(result.contains("标题：Rust 性能优化"));
        assert!(result.contains("char_indices()"));
        assert!(result.contains("Home"));
    }
}
