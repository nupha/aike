// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! HTML parsing and conversion — powered by `scraper` and `mdka`.
//!
//! Exposes:
//! - `parseHTML(html): Document`          — module-level function
//! - `html2markdown(html): string`        — module-level function
//! - `convert_html(html, mode)`           — shared utility for `HttpResponse.text()`
//! - `Document` / `Element`               — internal classes (used via module API)

use ego_tree::NodeId;
use linkme::distributed_slice;
use mdka::html_to_markdown;
use scraper::{ElementRef, Html, Selector};
use zuqe::{
    Array, CallContext, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

/// Recover the `NodeId` of an `ElementRef` by scanning the full tree.
fn find_node_id(html: &Html, target: &ElementRef) -> Option<NodeId> {
    let root = html.tree.root();
    for node in root.descendants() {
        if let Some(el) = ElementRef::wrap(node)
            && el == *target
        {
            return Some(node.id());
        }
    }
    None
}

/// Get an `ElementRef` from the tree by `NodeId`.
fn get_element_ref<'a>(html: &'a Html, id: NodeId) -> Option<ElementRef<'a>> {
    let node_ref = html.tree.get(id)?;
    ElementRef::wrap(node_ref)
}

/// Extract JS string from a JS argument.
fn arg_str(ctx: &mut ScopedContext, cc: &CallContext, idx: usize) -> Result<String, i32> {
    let s = cc.arg(idx).to_string(ctx)?;
    Ok(s.content().encode_utf8(false).to_string())
}

// ---------------------------------------------------------------------------
// HTML ↔ text conversion  (shared with `request.rs`)
// ---------------------------------------------------------------------------

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
                // from may not be a char boundary (multi-byte UTF-8),
                // walk forward to the nearest safe boundary
                let start = (from..=pos)
                    .find(|&i| lower.is_char_boundary(i))
                    .unwrap_or(pos);
                let slice = &lower[start..=pos];
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

/// HTML conversion mode for `convert_html()`.
#[derive(Clone, Copy)]
pub(crate) enum HtmlConvertMode {
    /// Strip all HTML tags, decode entities, return plain text.
    Strip,
    /// Convert HTML to Markdown via `mdka`.
    Markdown,
}

/// Shared HTML conversion — used both by `html2markdown()` and `HttpResponse.text()`.
pub(crate) fn convert_html(html: &str, mode: HtmlConvertMode) -> String {
    match mode {
        HtmlConvertMode::Markdown => html_to_markdown(html),
        HtmlConvertMode::Strip => strip_html_tags(html),
    }
}

// ---------------------------------------------------------------------------
// Module-level functions
// ---------------------------------------------------------------------------

/// `parseHTML(html: string): Document`  —  parse HTML into a queryable Document.
pub(crate) fn js_parse_html(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let html_str = arg_str(ctx, &cc, 0)?;
    let html = Html::parse_document(&html_str);
    HtmlDocument::new_instance(ctx, JSValue::UNDEFINED, HtmlDocumentPayload { html })
        .map(JSValue::from)
}

/// `html2markdown(html: string): string`  —  convert HTML to Markdown.
pub(crate) fn js_html2markdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let html_str = arg_str(ctx, &cc, 0)?;
    let md = html_to_markdown(&html_str);
    ctx.create_string_from_utf8(md.into_bytes())
        .map(JSValue::make_string)
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

pub(crate) struct HtmlDocumentPayload {
    html: Html,
}

impl GcTrace for HtmlDocumentPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

/// `this`-access helper for Document.
fn get_doc<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a HtmlDocumentPayload, i32> {
    HtmlDocument::payload_of(val).ok_or_else(|| ctx.throw_type_error("not a Document instance"))
}

fn js_document_query_selector_all(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let doc = get_doc(ctx, &this)?;
    let selector_str = arg_str(ctx, &cc, 0)?;

    let selector = Selector::parse(&selector_str)
        .map_err(|_| ctx.throw_internal_error("invalid CSS selector"))?;

    let mut results: Vec<JSValue> = Vec::new();
    for el_ref in doc.html.select(&selector) {
        if let Some(node_id) = find_node_id(&doc.html, &el_ref) {
            let el = HtmlElement::new_instance(
                ctx,
                JSValue::UNDEFINED,
                ElementPayload {
                    html: doc.html.clone(),
                    node_id,
                },
            )?;
            results.push(el.into());
        }
    }

    Array::from_values(ctx, results).map(JSValue::from)
}

const DOCUMENT_PROTO: &[FieldDef] = &[FieldDef::FUNC(
    "querySelectorAll",
    1,
    js_document_query_selector_all,
    0,
)];

def_native_object!(
    "Document",
    HtmlDocument,
    HtmlDocumentPayload,
    DOCUMENT_PROTO,
    None
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_document(rt: &mut Runtime) -> Result<ClassID, i32> {
    HtmlDocument::register(rt)
}

// ---------------------------------------------------------------------------
// Element
// ---------------------------------------------------------------------------

pub(crate) struct ElementPayload {
    html: Html,
    node_id: NodeId,
}

impl GcTrace for ElementPayload {
    fn trace(&self, _gcx: &mut GcTraceCtx) {}
}

/// `this`-access helper for Element.
fn get_element<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a ElementPayload, i32> {
    HtmlElement::payload_of(val).ok_or_else(|| ctx.throw_type_error("not an Element instance"))
}

// -- getters --

fn js_element_tag_name(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let el = get_element(ctx, &this)?;
    let tag = get_element_ref(&el.html, el.node_id)
        .map(|r| r.value().name.local.as_ref().to_string())
        .unwrap_or_default();
    ctx.create_string_from_utf8(tag.into_bytes())
        .map(JSValue::make_string)
}

fn js_element_text_content(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let el = get_element(ctx, &this)?;
    let text = get_element_ref(&el.html, el.node_id)
        .map(|r| r.text().collect::<String>())
        .unwrap_or_default();
    ctx.create_string_from_utf8(text.into_bytes())
        .map(JSValue::make_string)
}

fn js_element_children(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let el = get_element(ctx, &this)?;

    let node_ref = el
        .html
        .tree
        .get(el.node_id)
        .ok_or_else(|| ctx.throw_internal_error("element node not found in tree"))?;

    let mut results: Vec<JSValue> = Vec::new();
    for child in node_ref.children() {
        if ElementRef::wrap(child).is_some() {
            let child_el = HtmlElement::new_instance(
                ctx,
                JSValue::UNDEFINED,
                ElementPayload {
                    html: el.html.clone(),
                    node_id: child.id(),
                },
            )?;
            results.push(child_el.into());
        }
    }
    Array::from_values(ctx, results).map(JSValue::from)
}

// -- methods --

fn js_element_get_attribute(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let el = get_element(ctx, &this)?;
    let name = arg_str(ctx, &cc, 0)?;

    let val = get_element_ref(&el.html, el.node_id)
        .and_then(|r| r.value().attr(name.as_str()))
        .map(|s| s.to_string());

    match val {
        Some(s) => ctx
            .create_string_from_utf8(s.into_bytes())
            .map(JSValue::make_string),
        None => Ok(JSValue::NULL),
    }
}

fn js_element_query_selector(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let el = get_element(ctx, &this)?;
    let selector_str = arg_str(ctx, &cc, 0)?;

    let selector = Selector::parse(&selector_str)
        .map_err(|_| ctx.throw_internal_error("invalid CSS selector"))?;

    let el_ref = get_element_ref(&el.html, el.node_id)
        .ok_or_else(|| ctx.throw_internal_error("element not found in tree"))?;

    if let Some(child_ref) = el_ref.select(&selector).next() {
        if let Some(child_id) = find_node_id(&el.html, &child_ref) {
            return HtmlElement::new_instance(
                ctx,
                JSValue::UNDEFINED,
                ElementPayload {
                    html: el.html.clone(),
                    node_id: child_id,
                },
            )
            .map(|e| e.into());
        }
    }

    Ok(JSValue::NULL)
}

fn js_element_query_selector_all(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let el = get_element(ctx, &this)?;
    let selector_str = arg_str(ctx, &cc, 0)?;

    let selector = Selector::parse(&selector_str)
        .map_err(|_| ctx.throw_internal_error("invalid CSS selector"))?;

    let el_ref = get_element_ref(&el.html, el.node_id)
        .ok_or_else(|| ctx.throw_internal_error("element not found in tree"))?;

    let mut results: Vec<JSValue> = Vec::new();
    for child_ref in el_ref.select(&selector) {
        if let Some(child_id) = find_node_id(&el.html, &child_ref) {
            let child_el = HtmlElement::new_instance(
                ctx,
                JSValue::UNDEFINED,
                ElementPayload {
                    html: el.html.clone(),
                    node_id: child_id,
                },
            )?;
            results.push(child_el.into());
        }
    }

    Array::from_values(ctx, results).map(JSValue::from)
}

const ELEMENT_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("tagName", Some(js_element_tag_name), None, 0),
    FieldDef::GETSET("textContent", Some(js_element_text_content), None, 0),
    FieldDef::GETSET("children", Some(js_element_children), None, 0),
    FieldDef::FUNC("getAttribute", 1, js_element_get_attribute, 0),
    FieldDef::FUNC("querySelector", 1, js_element_query_selector, 0),
    FieldDef::FUNC("querySelectorAll", 1, js_element_query_selector_all, 0),
];

def_native_object!("Element", HtmlElement, ElementPayload, ELEMENT_PROTO, None);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_element(rt: &mut Runtime) -> Result<ClassID, i32> {
    HtmlElement::register(rt)
}
