// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
};

use crate::USER_CLASS_DEFS;

/// Case-insensitive header storage. Names keep their original casing; lookups
/// are case-insensitive per the Fetch/HTTP spec.
///
/// `Set-Cookie` values are stored separately because they MUST NOT be
/// comma-joined per the Fetch/HTTP spec (unlike all other headers).
#[derive(Clone, Default)]
pub(crate) struct HeaderList {
    list: Vec<(String, String)>,
    /// Separate storage for `Set-Cookie` values (never comma-merged).
    set_cookies: Vec<String>,
}

impl HeaderList {
    pub(crate) fn new() -> Self {
        HeaderList {
            list: Vec::new(),
            set_cookies: Vec::new(),
        }
    }

    pub(crate) fn from_vec(v: Vec<(String, String)>) -> Self {
        let mut h = HeaderList::new();
        for (name, value) in v {
            if name.eq_ignore_ascii_case("set-cookie") {
                h.set_cookies.push(value);
            } else {
                h.list.push((name, value));
            }
        }
        h
    }

    /// Append a value, combining with any existing same-name (case-insensitive)
    /// header as a comma-separated list. `Set-Cookie` is an exception — values
    /// are stored individually and never comma-merged.
    pub(crate) fn append(&mut self, name: &str, value: &str) {
        if name.eq_ignore_ascii_case("set-cookie") {
            self.set_cookies.push(value.to_string());
            return;
        }
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
    /// For `Set-Cookie`, replaces all existing values with a single entry.
    pub(crate) fn set(&mut self, name: &str, value: &str) {
        if name.eq_ignore_ascii_case("set-cookie") {
            self.set_cookies.clear();
            self.set_cookies.push(value.to_string());
            return;
        }
        let lower = name.to_lowercase();
        self.list.retain(|(k, _)| k.to_lowercase() != lower);
        self.list.push((name.to_string(), value.to_string()));
    }

    pub(crate) fn delete(&mut self, name: &str) {
        if name.eq_ignore_ascii_case("set-cookie") {
            self.set_cookies.clear();
            return;
        }
        let lower = name.to_lowercase();
        self.list.retain(|(k, _)| k.to_lowercase() != lower);
    }

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        if name.eq_ignore_ascii_case("set-cookie") {
            if self.set_cookies.is_empty() {
                return None;
            }
            return Some(self.set_cookies.join(", "));
        }
        let lower = name.to_lowercase();
        self.list
            .iter()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.clone())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        if name.eq_ignore_ascii_case("set-cookie") {
            return !self.set_cookies.is_empty();
        }
        let lower = name.to_lowercase();
        self.list.iter().any(|(k, _)| k.to_lowercase() == lower)
    }

    /// Entries as exposed to JS: sorted by lowercased name, per the WHATWG
    /// "sort and combine" algorithm. Multiple `Set-Cookie` values keep their
    /// relative insertion order (the sort is stable).
    pub(crate) fn entries(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .to_vec()
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        // Stable sort keeps the relative order of repeated `Set-Cookie` values.
        out.sort_by(|(a, _), (b, _)| a.cmp(b));
        out
    }

    /// Get all values for a given header name (case-insensitive).
    pub(crate) fn get_all(&self, name: &str) -> Vec<String> {
        if name.eq_ignore_ascii_case("set-cookie") {
            return self.set_cookies.clone();
        }
        let lower = name.to_lowercase();
        self.list
            .iter()
            .filter(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub(crate) fn to_vec(&self) -> Vec<(String, String)> {
        let mut out = self.list.clone();
        for v in &self.set_cookies {
            out.push(("set-cookie".to_string(), v.clone()));
        }
        out
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

/// WHATWG valid header value: a *byte* sequence containing no NUL, CR or LF.
/// Other control characters (notably TAB) are legal. Code points above U+00FF
/// cannot be represented as a header byte at all, so they are rejected too
/// (the JS string is a sequence of bytes, not UTF-8, at this layer).
fn is_valid_header_value(value: &str) -> bool {
    value
        .chars()
        .all(|c| !matches!(c, '\0' | '\r' | '\n') && (c as u32) <= 0xFF)
}

/// WHATWG "normalize a header value": strip leading/trailing HTTP whitespace
/// (only SP, TAB, CR, LF — *not* arbitrary Unicode whitespace like `str::trim`).
fn normalize_header_value(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'))
}

fn check_header_name(ctx: &mut ScopedContext, name: &str) -> JSResult {
    if !is_valid_header_name(name) {
        return Err(ctx.throw_type_error(&format!("Invalid header name: {}", name)));
    }
    Ok(JSValue::UNDEFINED)
}

fn check_header_value(ctx: &mut ScopedContext, value: &str) -> JSResult {
    if !is_valid_header_value(value) {
        return Err(ctx.throw_type_error("Invalid header value"));
    }
    Ok(JSValue::UNDEFINED)
}

/// Fetch spec "headers guard": controls whether/how mutation is allowed.
/// The `request` guard only adds forbidden-header filtering, which we do not
/// enforce, so it is not represented here.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Guard {
    None,
    /// Headers of an opaque/error `Response`: any mutation throws `TypeError`.
    Immutable,
    /// Headers of a normal `Response`: mutable, but forbidden response headers
    /// (`Set-Cookie`, `Set-Cookie2`) are silently dropped.
    Response,
    /// Headers of a `mode: "no-cors"` `Request`: mutations that are not
    /// CORS-safelisted are silently ignored (no error).
    RequestNoCors,
}

/// WHATWG "CORS-safelisted request-header". Only these may be set on a
/// `no-cors` request; everything else is dropped.
fn is_cors_safelisted(name: &str, value: &str) -> bool {
    // Values over 128 bytes are never safelisted, whatever the name.
    if value.len() > 128 {
        return false;
    }
    let lower = name.to_lowercase();
    match lower.as_str() {
        "accept" => !value.bytes().any(is_cors_unsafe_byte),
        "accept-language" | "content-language" => value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
                | b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')),
        "content-type" => {
            if value.bytes().any(is_cors_unsafe_byte) {
                return false;
            }
            // Essence (type/subtype without parameters) must be one of three.
            let essence = value.split(';').next().unwrap_or("").trim().to_lowercase();
            matches!(
                essence.as_str(),
                "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
            )
        }
        _ => false,
    }
}

/// WHATWG "CORS-unsafe request-header byte".
fn is_cors_unsafe_byte(b: u8) -> bool {
    b < 0x20 && b != b'\t'
        || matches!(
            b,
            b'"' | b'(' | b')' | b':' | b'<' | b'>' | b'?' | b'@' | b'[' | b'\\' | b']'
                | b'{' | b'}' | 0x7F
        )
}

pub(crate) struct HeadersPayload {
    pub(crate) list: HeaderList,
    pub(crate) guard: Guard,
}

impl HeadersPayload {
    /// A mutable, standalone header set (`new Headers()`, `Request.headers`).
    pub(crate) fn new(list: HeaderList) -> Self {
        HeadersPayload {
            list,
            guard: Guard::None,
        }
    }

    /// Header set exposed by a `Response`: mutation throws `TypeError`.
    pub(crate) fn immutable(list: HeaderList) -> Self {
        HeadersPayload {
            list,
            guard: Guard::Immutable,
        }
    }

    /// Header set of a `mode: "no-cors"` request: non-safelisted writes are
    /// silently dropped.
    pub(crate) fn no_cors(list: HeaderList) -> Self {
        HeadersPayload {
            list,
            guard: Guard::RequestNoCors,
        }
    }

    /// Header set of a normal `Response`: mutable except for forbidden
    /// response headers.
    pub(crate) fn response(list: HeaderList) -> Self {
        HeadersPayload {
            list,
            guard: Guard::Response,
        }
    }
}

/// WHATWG "forbidden response-header name": cannot be set through a Response's
/// headers, silently ignored under the `response` guard.
fn is_forbidden_response_header(name: &str) -> bool {
    let lower = name.to_lowercase();
    matches!(lower.as_str(), "set-cookie" | "set-cookie2")
}

/// Reject mutation on an immutable (response) header set, per the Fetch spec.
fn check_mutable(ctx: &mut ScopedContext, guard: Guard) -> JSResult {
    if guard == Guard::Immutable {
        return Err(ctx.throw_type_error("Headers are immutable"));
    }
    Ok(JSValue::UNDEFINED)
}

fn get_headers<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a HeadersPayload, i32> {
    JSHeaders::payload_of(val).ok_or_else(|| ctx.throw_type_error("not a Headers instance"))
}

fn get_headers_mut<'a>(
    ctx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut HeadersPayload, i32> {
    JSHeaders::payload_mut_of(val).ok_or_else(|| ctx.throw_type_error("not a Headers instance"))
}

/// Build a `HeaderList` from a JS init: a Headers instance, an array of
/// `[name, value]` pairs, or a plain record object.
pub(crate) fn header_list_from_js(
    ctx: &mut ScopedContext,
    val: &mut JSValue,
) -> Result<HeaderList, i32> {
    if let Some(h) = JSHeaders::payload_of(val) {
        return Ok(h.list.clone());
    }
    let mut list = HeaderList::new();
    // WebIDL: an omitted/`undefined` init means "no headers", but `null` is not
    // a valid `HeadersInit` and must be rejected.
    if val.is_undefined() {
        return Ok(list);
    }
    if val.is_null() {
        return Err(ctx.throw_type_error("Headers init must not be null"));
    }
    if let Some(mut obj) = val.as_object() {
        let pks =
            obj.get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK)?;
        for pk in pks.iter() {
            let elem = obj.get_property(ctx, pk.atom)?;
            if elem.is_array(ctx)? {
                // A sequence init entry must be exactly a [name, value] pair.
                let len = elem
                    .get_property_str(ctx, "length")?
                    .to_number(ctx)?
                    .to_i32();
                if len != 2 {
                    return Err(ctx.throw_type_error(
                        "Headers init sequence entry must contain exactly two items",
                    ));
                }
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
                let value = normalize_header_value(&value);
                check_header_value(ctx, value)?;
                list.append(&name, value);
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
                let value = normalize_header_value(&value);
                check_header_value(ctx, value)?;
                list.append(&key, value);
            }
        }
        Ok(list)
    } else {
        Err(ctx.throw_type_error("invalid Headers init"))
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
    JSHeaders::new_instance(ctx, new_target, HeadersPayload::new(list)).map(JSValue::from)
}

fn js_headers_append(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let guard = get_headers(ctx, &this)?.guard;
    check_mutable(ctx, guard)?;
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
    let value = normalize_header_value(&value);
    check_header_value(ctx, value)?;
    // response guard: forbidden response headers are silently dropped.
    if guard == Guard::Response && is_forbidden_response_header(&name) {
        return Ok(JSValue::UNDEFINED);
    }
    // no-cors guard: the safelist test applies to the *combined* value that
    // append would produce, not just the new one.
    if guard == Guard::RequestNoCors {
        let combined = match h.list.get(&name) {
            Some(existing) => format!("{existing}, {value}"),
            None => value.to_string(),
        };
        if !is_cors_safelisted(&name, &combined) {
            return Ok(JSValue::UNDEFINED);
        }
    }
    h.list.append(&name, value);
    Ok(JSValue::UNDEFINED)
}

fn js_headers_delete(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let guard = get_headers(ctx, &this)?.guard;
    check_mutable(ctx, guard)?;
    let h = get_headers_mut(ctx, &mut this)?;
    let name = cc
        .arg(0)
        .to_string(ctx)?
        .content()
        .encode_utf8(false)
        .to_string();
    check_header_name(ctx, &name)?;
    // response guard: forbidden response headers cannot be removed either.
    if guard == Guard::Response && is_forbidden_response_header(&name) {
        return Ok(JSValue::UNDEFINED);
    }
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
        Some(v) => ctx
            .create_string_from_utf8(v.into_bytes())
            .map(JSValue::make_string),
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
    Ok(JSValue::from(h.list.has(&name)))
}

fn js_headers_set(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let guard = get_headers(ctx, &this)?.guard;
    check_mutable(ctx, guard)?;
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
    let value = normalize_header_value(&value);
    check_header_value(ctx, value)?;
    // response guard: forbidden response headers are silently dropped.
    if guard == Guard::Response && is_forbidden_response_header(&name) {
        return Ok(JSValue::UNDEFINED);
    }
    // no-cors guard: drop writes that are not CORS-safelisted.
    if guard == Guard::RequestNoCors && !is_cors_safelisted(&name, value) {
        return Ok(JSValue::UNDEFINED);
    }
    h.list.set(&name, value);
    Ok(JSValue::UNDEFINED)
}

/// `keys()`/`values()`/`entries()` must return an iterator, not an array.
/// Snapshot the values into an array and hand back its array iterator.
fn values_iterator(ctx: &mut ScopedContext, values: Vec<JSValue>) -> JSResult {
    let arr = Array::from_values(ctx, values)?;
    let iter_fn = arr.get_property_str(ctx, "[Symbol.iterator]")?;
    let cb = iter_fn.ensure_callable(ctx)?;
    cb.call(ctx, arr.into(), &[])
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
                .map(JSValue::make_string)
        })
        .collect::<Result<Vec<_>, _>>()?;
    values_iterator(ctx, names)
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
                .map(JSValue::make_string)
        })
        .collect::<Result<Vec<_>, _>>()?;
    values_iterator(ctx, vals)
}

fn js_headers_entries(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let mut pairs: Vec<JSValue> = Vec::new();
    for (k, v) in h.list.entries() {
        let k_js = ctx.create_string_from_utf8(k.as_bytes().to_vec())?;
        let v_js = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        pairs.push(
            Array::from_values(
                ctx,
                vec![JSValue::make_string(k_js), JSValue::make_string(v_js)],
            )?
            .into(),
        );
    }
    values_iterator(ctx, pairs)
}

fn js_headers_for_each(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    if cc.length() == 0 {
        return Err(ctx.throw_type_error("forEach requires a callback"));
    }
    let cb = cc.arg(0).ensure_callable(ctx)?;
    let this_arg = if cc.length() > 1 {
        cc.arg(1)
    } else {
        JSValue::UNDEFINED
    };
    for (k, v) in h.list.entries() {
        let k_js = ctx.create_string_from_utf8(k.as_bytes().to_vec())?;
        let v_js = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
        cb.call(
            ctx,
            this_arg,
            &[JSValue::make_string(v_js), JSValue::make_string(k_js), this],
        )?;
    }
    Ok(JSValue::UNDEFINED)
}

/// `getSetCookie()` — returns all `Set-Cookie` header values as an array.
fn js_headers_get_set_cookie(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let h = get_headers(ctx, &this)?;
    let cookies: Vec<JSValue> = h
        .list
        .get_all("set-cookie")
        .into_iter()
        .map(|v| {
            ctx.create_string_from_utf8(v.into_bytes())
                .map(JSValue::make_string)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Array::from_values(ctx, cookies).map(JSValue::from)
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
        let pair = Array::from_values(
            ctx,
            vec![JSValue::make_string(k_js), JSValue::make_string(v_js)],
        )?;
        entries.push(pair.into());
    }
    values_iterator(ctx, entries)
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
    FieldDef::FUNC("getSetCookie", 0, js_headers_get_set_cookie, 0),
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
