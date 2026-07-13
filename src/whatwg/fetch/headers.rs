// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG Fetch — Headers class and shared header list.
//!
//! `HeaderList` is the single source of truth for HTTP headers, reused by
//! the client `Response`, the `Request`, and (via helpers) the server side.

use std::cell::Cell;

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

/// Case-insensitive header storage. Names keep their original casing; lookups
/// are case-insensitive per the Fetch/HTTP spec.
#[derive(Clone, Default)]
pub(crate) struct HeaderList {
    list: Vec<(String, String)>,
}

impl HeaderList {
    pub(crate) fn new() -> Self {
        HeaderList { list: Vec::new() }
    }

    pub(crate) fn from_vec(v: Vec<(String, String)>) -> Self {
        HeaderList { list: v }
    }

    /// Append a value, combining with any existing same-name (case-insensitive)
    /// header as a comma-separated list.
    pub(crate) fn append(&mut self, name: &str, value: &str) {
        let lower = name.to_lowercase();
        if let Some(pos) = self
            .list
            .iter()
            .position(|(k, _)| k.to_lowercase() == lower)
        {
            self.list[pos].1.push_str(", ");
            self.list[pos].1.push_str(value);
        } else {
            self.list.push((name.to_string(), value.to_string()));
        }
    }

    /// Set a value, replacing any existing same-name header.
    pub(crate) fn set(&mut self, name: &str, value: &str) {
        let lower = name.to_lowercase();
        self.list.retain(|(k, _)| k.to_lowercase() != lower);
        self.list.push((name.to_string(), value.to_string()));
    }

    pub(crate) fn delete(&mut self, name: &str) {
        let lower = name.to_lowercase();
        self.list.retain(|(k, _)| k.to_lowercase() != lower);
    }

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        let lower = name.to_lowercase();
        self.list
            .iter()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.clone())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.list.iter().any(|(k, _)| k.to_lowercase() == lower)
    }

    pub(crate) fn entries(&self) -> &[(String, String)] {
        &self.list
    }

    pub(crate) fn to_vec(&self) -> Vec<(String, String)> {
        self.list.clone()
    }
}

/// WHATWG valid header name = 1+ `tchar` (token chars).
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            matches!(b,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9'
            | b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*'
            | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
}

/// WHATWG valid header value: no control characters (0x00-0x1F, 0x7F).
fn is_valid_header_value(value: &str) -> bool {
    !value.bytes().any(|b| b <= 0x1F || b == 0x7F)
}

fn check_header_name(ctx: &mut ScopedContext, name: &str) -> JSResult {
    if !is_valid_header_name(name) {
        ctx.throw_type_error(&format!("Invalid header name: {}", name));
        return Err(-1);
    }
    Ok(JSValue::UNDEFINED)
}

fn check_header_value(ctx: &mut ScopedContext, value: &str) -> JSResult {
    if !is_valid_header_value(value) {
        ctx.throw_type_error("Invalid header value");
        return Err(-1);
    }
    Ok(JSValue::UNDEFINED)
}

pub(crate) struct HeadersPayload {
    pub(crate) list: HeaderList,
}

fn get_headers<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a HeadersPayload, i32> {
    val.as_native_object::<JSHeaders>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_type_error("not a Headers instance");
            -1
        })
}

fn get_headers_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HeadersPayload, i32> {
    val.as_native_object_mut::<JSHeaders>()
        .and_then(|o| o.payload_mut())
        .ok_or_else(|| {
            ctx.throw_type_error("not a Headers instance");
            -1
        })
}

/// Build a `HeaderList` from a JS init: a Headers instance, an array of
/// `[name, value]` pairs, or a plain record object.
pub(crate) fn header_list_from_js(
    ctx: &mut ScopedContext,
    val: &mut JSValue,
) -> Result<HeaderList, i32> {
    if let Some(h) = val
        .as_native_object::<JSHeaders>()
        .and_then(|o| o.payload())
    {
        return Ok(h.list.clone());
    }
    let mut list = HeaderList::new();
    if val.is_undefined_or_null() {
        return Ok(list);
    }
    if let Some(obj) = val.as_object_mut() {
        let pks =
            obj.get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK)?;
        for pk in pks.iter() {
            let elem = obj.get_property(ctx, pk.atom)?;
            if elem.is_array(ctx)? {
                let name = elem
                    .get_property_str(ctx, "0")?
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                let value = elem
                    .get_property_str(ctx, "1")?
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                check_header_name(ctx, &name)?;
                check_header_value(ctx, &value)?;
                list.append(&name, &value);
            } else {
                let key = pk
                    .atom
                    .to_value(ctx)?
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                let value = elem
                    .to_string(ctx)?
                    .content()
                    .encode_utf8(false)
                    .to_string();
                check_header_name(ctx, &key)?;
                check_header_value(ctx, &value)?;
                list.append(&key, &value);
            }
        }
        Ok(list)
    } else {
        ctx.throw_type_error("invalid Headers init");
        Err(-1)
    }
}

fn js_headers_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    let list = if cc.length() > 0 {
        let mut arg0 = cc.arg(0);
        header_list_from_js(ctx, &mut arg0)?
    } else {
        HeaderList::new()
    };
    JSHeaders::new_instance(ctx, new_target, HeadersPayload { list }).map(JSValue::from)
}

fn js_headers_append(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let h = get_headers_mut(ctx, &mut this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let value = cc
        .arg(1)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    check_header_name(ctx, &name)?;
    let value = value.trim().to_string();
    check_header_value(ctx, &value)?;
    h.list.append(&name, &value);
    Ok(JSValue::UNDEFINED)
}

fn js_headers_delete(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let h = get_headers_mut(ctx, &mut this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    check_header_name(ctx, &name)?;
    h.list.delete(&name);
    Ok(JSValue::UNDEFINED)
}

fn js_headers_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    check_header_name(ctx, &name)?;
    match h.list.get(&name) {
        Some(v) => {
            let s = ctx.create_string_from_utf8(v.into_bytes())?;
            Ok(s.into())
        }
        None => Ok(JSValue::NULL),
    }
}

fn js_headers_has(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    check_header_name(ctx, &name)?;
    Ok(JSValue::Bool(h.list.has(&name)))
}

fn js_headers_set(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let h = get_headers_mut(ctx, &mut this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    let value = cc
        .arg(1)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    check_header_name(ctx, &name)?;
    let value = value.trim().to_string();
    check_header_value(ctx, &value)?;
    h.list.set(&name, &value);
    Ok(JSValue::UNDEFINED)
}

fn js_headers_keys(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let names: Vec<JSValue> = h
        .list
        .entries()
        .iter()
        .map(|(k, _)| {
            ctx.create_string_from_utf8(k.as_bytes().to_vec())
                .map(JSValue::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Array::from_values(ctx, names).map(JSValue::from)
}

fn js_headers_values(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let vals: Vec<JSValue> = h
        .list
        .entries()
        .iter()
        .map(|(_, v)| {
            ctx.create_string_from_utf8(v.as_bytes().to_vec())
                .map(JSValue::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Array::from_values(ctx, vals).map(JSValue::from)
}

fn js_headers_entries(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let mut pairs: Vec<JSValue> = Vec::new();
    for (k, v) in h.list.entries() {
        let k_js = ctx.create_string_from_utf8(k.as_bytes().to_vec())?;
        let v_js = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        pairs.push(Array::from_values(ctx, vec![k_js.into(), v_js.into()])?.into());
    }
    Array::from_values(ctx, pairs).map(JSValue::from)
}

fn js_headers_for_each(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    if cc.length() == 0 {
        ctx.throw_type_error("forEach requires a callback");
        return Err(-1);
    }
    let cb = cc.arg(0).into_callable(ctx)?;
    let this_arg = if cc.length() > 1 {
        cc.arg(1)
    } else {
        JSValue::UNDEFINED
    };
    for (k, v) in h.list.entries() {
        let k_js = ctx.create_string_from_utf8(k.as_bytes().to_vec())?;
        let v_js = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        cb.call(ctx, this_arg, &[v_js.into(), k_js.into(), this])?;
    }
    Ok(JSValue::UNDEFINED)
}

/// `[Symbol.iterator]` — yields `[name, value]` pairs so that
/// `for (const [k, v] of headers)` works.
fn js_headers_iterator(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let mut entries: Vec<JSValue> = Vec::new();
    for (k, v) in h.list.entries() {
        let k_js = ctx.create_string_from_utf8(k.as_bytes().to_vec())?;
        let v_js = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        let pair = Array::from_values(ctx, vec![k_js.into(), v_js.into()])?;
        entries.push(pair.into());
    }
    let arr = Array::from_values(ctx, entries)?;
    let iter_fn = arr.get_property_str(ctx, "[Symbol.iterator]")?;
    let cb = iter_fn.into_callable(ctx)?;
    cb.call(ctx, arr.into(), &[])
}

const HEADERS_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("append", 2, js_headers_append, 0),
    FieldDef::FUNC("delete", 1, js_headers_delete, 0),
    FieldDef::FUNC("get", 1, js_headers_get, 0),
    FieldDef::FUNC("has", 1, js_headers_has, 0),
    FieldDef::FUNC("set", 2, js_headers_set, 0),
    FieldDef::FUNC("forEach", 1, js_headers_for_each, 0),
    FieldDef::FUNC("keys", 0, js_headers_keys, 0),
    FieldDef::FUNC("values", 0, js_headers_values, 0),
    FieldDef::FUNC("entries", 0, js_headers_entries, 0),
    FieldDef::FUNC("[Symbol.iterator]", 0, js_headers_iterator, 0),
];

def_native_object!(
    "Headers",
    JSHeaders,
    HeadersPayload,
    HEADERS_PROTO,
    Some(ClassCtorDef::new(js_headers_constructor, 1, true).expose_to_global())
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_headers(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSHeaders::register(rt)
}
