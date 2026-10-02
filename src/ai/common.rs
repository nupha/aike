// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Configuration shared by every `ai` remote client.
//!
//! `LlmClient` and `System1Client` talk to different endpoints and return
//! different shapes, but they are configured identically: a base URL, a
//! bearer token, a header map, a timeout and one bound HTTP client. That much
//! lives here once, and each class embeds it as `common`.
//!
//! The per-class extras (`maxTokens`/`extraBody` for LlmClient) stay on the
//! class's own payload rather than in an enum variant: no code needs to handle
//! "either kind of client" at runtime, so the compiler should keep the extras
//! apart instead of a `match`.

use std::{collections::HashMap, time::Duration};

use zuqe::{CallContext, JSResult, JSValue, ScopedContext};

use super::{JSLlmClient, JSSystem1Client};
use zuqe::NativeObject;

/// Getter magic values for the properties both clients expose.
///
/// A `FieldDef::GETSET` magic is private to its class and invisible to JS, so
/// the numbers are ours to keep consistent — but they must be shared: one
/// `common_getter` serves both classes, and the same number has to mean the
/// same property on both. Class-specific extras continue from `EXTRA_BASE`.
pub(crate) const MAGIC_MODEL: i32 = 0;
pub(crate) const MAGIC_BASE_URL: i32 = 1;
pub(crate) const MAGIC_TIMEOUT: i32 = 2;
pub(crate) const MAGIC_HEADERS: i32 = 3;
/// First magic available to class-specific properties.
pub(crate) const MAGIC_EXTRA_BASE: i32 = 4;

/// Connection configuration common to every remote client.
#[derive(Debug)]
pub(crate) struct ClientCommon {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) timeout: Duration,
    /// Shared reqwest client: built once, reused across requests so the
    /// connection pool / keep-alive survives between calls.
    pub(crate) http: reqwest::Client,
    pub(crate) model_name: String,
}

/// Borrow the common config of any client instance.
///
/// `NativeObject::payload_of` matches `class_id` exactly, so a class's own
/// accessor cannot see a sibling class's instance and no shared accessor can
/// name a single class. Enumerating them here is the same trade the MCP
/// classes make in `mcp/class_mcp.rs`.
fn get_common<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a ClientCommon, i32> {
    if let Some(p) = JSLlmClient::payload_of(val) {
        return Ok(&p.common);
    }
    if let Some(p) = JSSystem1Client::payload_of(val) {
        return Ok(&p.common);
    }
    Err(ctx.throw_type_error("not an ai client instance"))
}

fn get_common_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut ClientCommon, i32> {
    // Read the class id before re-borrowing `val` mutably: `payload_mut_of`
    // would otherwise hold the borrow across the second attempt.
    let mut not_a_client = || ctx.throw_type_error("not an ai client instance");
    let Some(obj) = val.as_object() else {
        return Err(not_a_client());
    };
    match obj.class_id() {
        id if id == JSLlmClient::class_id() => JSLlmClient::payload_mut_of(val)
            .map(|p| &mut p.common)
            .ok_or_else(not_a_client),
        id if id == JSSystem1Client::class_id() => JSSystem1Client::payload_mut_of(val)
            .map(|p| &mut p.common)
            .ok_or_else(not_a_client),
        _ => Err(not_a_client()),
    }
}

/// Build the `headers` snapshot returned by the `headers` getter.
fn headers_to_js(ctx: &mut ScopedContext, headers: &HashMap<String, String>) -> JSResult {
    let mut obj = ctx.create_base_object()?;
    for (k, v) in headers {
        let val = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        obj.set_property_str(ctx, k, JSValue::make_string(val))?;
    }
    Ok(obj.into())
}

/// Shared half of a class's property getter: answers the properties listed in
/// the magic constants above and returns `None` for the class's own extras, so
/// the caller can handle those itself.
pub(crate) fn common_getter(
    ctx: &mut ScopedContext,
    common: &ClientCommon,
    magic: i32,
) -> Result<Option<JSResult>, i32> {
    let r = match magic {
        MAGIC_MODEL => ctx
            .create_string_from_utf8(common.model_name.as_bytes().to_vec())
            .map(JSValue::make_string),
        MAGIC_BASE_URL => ctx
            .create_string_from_utf8(common.base_url.as_bytes().to_vec())
            .map(JSValue::make_string),
        MAGIC_TIMEOUT => Ok(JSValue::from(common.timeout.as_secs() as i32)),
        MAGIC_HEADERS => headers_to_js(ctx, &common.headers),
        _ => return Ok(None),
    };
    Ok(Some(r))
}

/// Getter for the shared properties. Serves every client class, so a class
/// whose properties are all shared (`System1Client`) points straight at this;
/// `LlmClient` points its shared properties here too and handles only its own
/// extras in a separate getter. The two never overlap: extras start at
/// `MAGIC_EXTRA_BASE`.
pub(crate) fn js_common_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let common = get_common(ctx, &this)?;
    match common_getter(ctx, common, cc.magic())? {
        Some(r) => r,
        None => Ok(JSValue::UNDEFINED),
    }
}

/// The `model`/`baseUrl`/`timeout`/`headers` GETSET entries, listed by each
/// class's proto table against these same magics. Not shared as a
/// `&[FieldDef]`: `FieldDef` is not `Copy`, so a proto table cannot move
/// entries out of a shared slice, and a macro would need every name it
/// references imported at each call site to save four lines. The magic
/// constants and `js_common_getter` are what must not drift; the property
/// names are written out.

/// Read a required string argument of a header accessor.
fn name_arg(ctx: &mut ScopedContext, v: JSValue, label: &str) -> Result<String, i32> {
    v.as_string(true)
        .map(|s| s.content().encode_utf8(false).to_string())
        .ok_or_else(|| ctx.throw_type_error(&format!("{label} must be a string")))
}

/// Insert or update a single HTTP header. `undefined` is a no-op, `null`
/// removes the entry.
pub(crate) fn js_client_set_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let name = name_arg(ctx, cc.arg(0), "setHeader: header name")?;
    let val = cc.arg(1);

    if val.is_undefined() {
        return Ok(JSValue::UNDEFINED);
    }
    if val.is_null() {
        get_common_mut(ctx, &mut this)?.headers.remove(&name);
        return Ok(JSValue::UNDEFINED);
    }
    let s = name_arg(
        ctx,
        val,
        "setHeader: value must be a string, null, or undefined",
    )?;
    get_common_mut(ctx, &mut this)?.headers.insert(name, s);
    Ok(JSValue::UNDEFINED)
}

/// Read a single HTTP header value, or `undefined` when not set.
pub(crate) fn js_client_get_header(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let name = name_arg(ctx, cc.arg(0), "getHeader: header name")?;
    match get_common(ctx, &this)?.headers.get(&name) {
        Some(v) => ctx
            .create_string_from_utf8(v.as_bytes().to_vec())
            .map(JSValue::make_string),
        None => Ok(JSValue::UNDEFINED),
    }
}
