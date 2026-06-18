// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! WHATWG URL Standard — URL and URLSearchParams implementations.
//!
//! Both classes are backed by the `url` crate for spec-compliant parsing.
//! `URL.searchParams` returns a `URLSearchParams` that shares state via `Rc<RefCell<url::Url>>`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use linkme::distributed_slice;
use url::Url;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Object, Runtime,
    ScopedContext, def_native_object,
};

use crate::USER_CLASS_DEFS;

// ===========================================================================
// URL class
// ===========================================================================

pub(crate) struct UrlPayload {
    /// Shared Url state (shared with URLSearchParams when created).
    shared: Rc<RefCell<Url>>,
    /// Lazily created URLSearchParams tied to this URL.
    search_params: RefCell<Option<Object>>,
}

fn get_url<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a UrlPayload, i32> {
    val.as_native_object::<JSURL>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_internal_error("not a URL instance");
            -1
        })
}

// -- Constructor -----------------------------------------------------------

fn js_url_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();
    if cc.length() < 1 {
        ctx.throw_type_error("Failed to construct 'URL': 1 argument required");
        return Err(-1);
    }

    let url_str = cc.arg(0).to_string(ctx)?;
    let url_text = url_str.content().encode_utf8(false).to_string();

    let parsed = if cc.length() >= 2 {
        let base_str = cc.arg(1).to_string(ctx)?;
        let base_text = base_str.content().encode_utf8(false).to_string();
        let base = Url::parse(&base_text).map_err(|_| {
            ctx.throw_type_error("Failed to construct 'URL': Invalid base URL");
            -1
        })?;
        base.join(&url_text).map_err(|_| {
            ctx.throw_type_error("Failed to construct 'URL': Invalid URL");
            -1
        })
    } else {
        Url::parse(&url_text).map_err(|_| {
            ctx.throw_type_error("Failed to construct 'URL': Invalid URL");
            -1
        })
    }?;

    JSURL::new_instance(
        ctx,
        new_target,
        UrlPayload {
            shared: Rc::new(RefCell::new(parsed)),
            search_params: RefCell::new(None),
        },
    )
    .map(JSValue::from)
}

// -- Getters / Setters -----------------------------------------------------

fn js_url_href_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let s = url.shared.borrow().as_str().to_string();
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_href_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let new_url = Url::parse(&text).map_err(|_| {
        ctx.throw_type_error("Failed to set 'href': Invalid URL");
        -1
    })?;
    *url.shared.borrow_mut() = new_url;
    // Invalidate cached searchParams on href change
    *url.search_params.borrow_mut() = None;
    Ok(JSValue::UNDEFINED)
}

fn js_url_origin_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let origin = match u.origin() {
        url::Origin::Opaque(_) => "null".to_string(),
        url::Origin::Tuple(scheme, host, port) => {
            if port > 0 {
                format!("{}://{}:{}", scheme, host, port)
            } else {
                format!("{}://{}", scheme, host)
            }
        }
    };
    let js = ctx.create_string_from_utf8(origin.into_bytes())?;
    Ok(js.into())
}

fn js_url_protocol_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = format!("{}:", u.scheme());
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_protocol_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let scheme = text.trim_end_matches(':');
    let mut u = url.shared.borrow_mut();
    let _ = u.set_scheme(scheme);
    Ok(JSValue::UNDEFINED)
}

fn js_url_username_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = u.username().to_string();
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_username_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    let _ = u.set_username(&text);
    Ok(JSValue::UNDEFINED)
}

fn js_url_password_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = u.password().unwrap_or("").to_string();
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_password_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    let _ = u.set_password(if text.is_empty() { None } else { Some(&text) });
    Ok(JSValue::UNDEFINED)
}

fn js_url_host_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = match (u.host_str(), u.port()) {
        (Some(host), Some(port)) => format!("{}:{}", host, port),
        (Some(host), None) => host.to_string(),
        _ => String::new(),
    };
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_host_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    let _ = u.set_host(Some(&text));
    Ok(JSValue::UNDEFINED)
}

fn js_url_hostname_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = u.host_str().unwrap_or("").to_string();
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_hostname_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    let _ = u.set_host(Some(&text));
    Ok(JSValue::UNDEFINED)
}

fn js_url_port_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = match u.port() {
        Some(port) => port.to_string(),
        None => String::new(),
    };
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_port_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    if text.is_empty() {
        let _ = u.set_port(None);
    } else if let Ok(port) = text.parse::<u16>() {
        let _ = u.set_port(Some(port));
    }
    Ok(JSValue::UNDEFINED)
}

fn js_url_pathname_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = u.path().to_string();
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_pathname_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    u.set_path(&text);
    Ok(JSValue::UNDEFINED)
}

fn js_url_search_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = match u.query() {
        Some(q) if !q.is_empty() => format!("?{}", q),
        _ => String::new(),
    };
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_search_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    let query = text.strip_prefix('?').unwrap_or(&text);
    if query.is_empty() {
        u.set_query(None);
    } else {
        u.set_query(Some(query));
    }
    Ok(JSValue::UNDEFINED)
}

fn js_url_hash_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let u = url.shared.borrow();
    let s = match u.fragment() {
        Some(f) if !f.is_empty() => format!("#{}", f),
        _ => String::new(),
    };
    let js = ctx.create_string_from_utf8(s.into_bytes())?;
    Ok(js.into())
}

fn js_url_hash_setter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;
    let val = cc.arg(0).to_string(ctx)?;
    let text = val.content().encode_utf8(false).to_string();
    let mut u = url.shared.borrow_mut();
    let fragment = text.strip_prefix('#').unwrap_or(&text);
    if fragment.is_empty() {
        u.set_fragment(None);
    } else {
        u.set_fragment(Some(fragment));
    }
    Ok(JSValue::UNDEFINED)
}

// -- searchParams getter (lazy, shares Rc<RefCell<Url>>) ------------------

fn js_url_search_params_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let url = get_url(ctx, &this)?;

    // Return cached searchParams if already created
    if let Some(sp) = url.search_params.borrow().as_ref() {
        return Ok(sp.clone().into());
    }

    let sp = JSURLSearchParams::new_instance(
        ctx,
        JSValue::UNDEFINED,
        SearchParamsPayload {
            shared_url: Some(url.shared.clone()),
            standalone_query: RefCell::new(String::new()),
        },
    )?;
    *url.search_params.borrow_mut() = Some(sp.as_object().clone());
    Ok(sp.into())
}

// -- toString / toJSON -----------------------------------------------------

fn js_url_to_string(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    js_url_href_getter(ctx, cc)
}

// ===========================================================================
// URLSearchParams
// ===========================================================================

struct SearchParamsPayload {
    /// Shared URL state; None when standalone (not tied to a URL).
    shared_url: Option<Rc<RefCell<Url>>>,
    /// Query string (used only when standalone).
    standalone_query: RefCell<String>,
}

fn get_sp<'a>(ctx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a SearchParamsPayload, i32> {
    val.as_native_object::<JSURLSearchParams>()
        .and_then(|o| o.payload())
        .ok_or_else(|| {
            ctx.throw_internal_error("not a URLSearchParams instance");
            -1
        })
}

/// Get current query string from shared URL or standalone.
fn sp_get_query(sp: &SearchParamsPayload) -> String {
    match sp.shared_url {
        Some(ref u) => u.borrow().query().unwrap_or("").to_string(),
        None => sp.standalone_query.borrow().clone(),
    }
}

/// Set current query string on shared URL and/or standalone.
fn sp_set_query(sp: &SearchParamsPayload, query: &str) {
    if let Some(ref u) = sp.shared_url {
        if query.is_empty() {
            u.borrow_mut().set_query(None);
        } else {
            u.borrow_mut().set_query(Some(query));
        }
    } else {
        *sp.standalone_query.borrow_mut() = query.to_string();
    }
}

// -- URLSearchParams constructor -------------------------------------------

fn js_sp_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let new_target = cc.this();

    let query = if cc.length() == 0 {
        String::new()
    } else {
        let arg = &cc.arg(0);
        match arg {
            JSValue::String(s) => {
                let text = s.content().encode_utf8(false).to_string();
                text.strip_prefix('?').unwrap_or(&text).to_string()
            }
            JSValue::Object(obj_ref) => {
                let mut o = obj_ref.clone();
                let pks = o.get_own_property_names(
                    ctx,
                    zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK,
                )?;
                let mut pairs: Vec<(String, String)> = Vec::new();
                for pk in pks.iter() {
                    let key_v = pk.atom.to_value(ctx)?;
                    let key_r = key_v.to_string(ctx)?;
                    let k = key_r.content().encode_utf8(false).to_string();
                    let val = o.get_property(ctx, pk.atom)?;
                    let v = val.to_string(ctx)?.content().encode_utf8(false).to_string();
                    let encoded_k =
                        url::form_urlencoded::byte_serialize(k.as_bytes()).collect::<String>();
                    let encoded_v =
                        url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();
                    pairs.push((encoded_k, encoded_v));
                }
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                pairs
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<_>>()
                    .join("&")
            }
            _ => {
                ctx.throw_type_error("URLSearchParams init must be string or object");
                return Err(-1);
            }
        }
    };

    JSURLSearchParams::new_instance(
        ctx,
        new_target,
        SearchParamsPayload {
            shared_url: None,
            standalone_query: RefCell::new(query),
        },
    )
    .map(JSValue::from)
}

// -- URLSearchParams methods ----------------------------------------------

fn js_sp_append(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let name = cc.arg(0).to_string(ctx)?;
    let value = cc.arg(1).to_string(ctx)?;
    let n = name.content().encode_utf8(false);
    let v = value.content().encode_utf8(false);
    let encoded_n = url::form_urlencoded::byte_serialize(n.as_bytes()).collect::<String>();
    let encoded_v = url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();

    let mut query = sp_get_query(sp);
    if query.is_empty() {
        query = format!("{}={}", encoded_n, encoded_v);
    } else {
        query.push_str(&format!("&{}={}", encoded_n, encoded_v));
    }
    sp_set_query(sp, &query);

    Ok(JSValue::UNDEFINED)
}

fn js_sp_delete(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let name = cc.arg(0).to_string(ctx)?;
    let n = name.content().encode_utf8(false).to_string();

    let query = sp_get_query(sp);
    let pairs: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k.to_string() != n)
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();
    sp_set_query(sp, &pairs.join("&"));

    Ok(JSValue::UNDEFINED)
}

fn js_sp_get(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let name = cc.arg(0).to_string(ctx)?;
    let n = name.content().encode_utf8(false).to_string();

    let query = sp_get_query(sp);
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        if k.to_string() == n {
            let s = ctx.create_string_from_utf8(v.as_bytes().to_vec())?;
            return Ok(s.into());
        }
    }

    Ok(JSValue::NULL)
}

fn js_sp_get_all(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let name = cc.arg(0).to_string(ctx)?;
    let n = name.content().encode_utf8(false).to_string();

    let query = sp_get_query(sp);
    let values: Vec<JSValue> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, _)| k.to_string() == n)
        .map(|(_, v)| {
            let s = ctx.create_string_from_utf8(v.as_bytes().to_vec()).unwrap();
            JSValue::String(s)
        })
        .collect();

    let arr = Array::from_values(ctx, values)?;
    Ok(arr.into())
}

fn js_sp_has(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let name = cc.arg(0).to_string(ctx)?;
    let n = name.content().encode_utf8(false).to_string();

    let query = sp_get_query(sp);
    let found = url::form_urlencoded::parse(query.as_bytes()).any(|(k, _)| k.to_string() == n);

    Ok(JSValue::Bool(found))
}

fn js_sp_set(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let name = cc.arg(0).to_string(ctx)?;
    let value = cc.arg(1).to_string(ctx)?;
    let n = name.content().encode_utf8(false);
    let v = value.content().encode_utf8(false);
    let encoded_n = url::form_urlencoded::byte_serialize(n.as_bytes()).collect::<String>();
    let encoded_v = url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>();

    let query = sp_get_query(sp);
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, val)| (k.to_string(), val.to_string()))
        .collect();

    let mut new_pairs = Vec::new();
    let mut replaced = false;
    for (k, val) in &pairs {
        if k == &n {
            if !replaced {
                new_pairs.push((encoded_n.clone(), encoded_v.clone()));
                replaced = true;
            }
        } else {
            new_pairs.push((k.clone(), val.clone()));
        }
    }
    if !replaced {
        new_pairs.push((encoded_n, encoded_v));
    }

    let new_query = new_pairs
        .iter()
        .map(|(k, val)| format!("{}={}", k, val))
        .collect::<Vec<_>>()
        .join("&");
    sp_set_query(sp, &new_query);

    Ok(JSValue::UNDEFINED)
}

fn js_sp_sort(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;

    let query = sp_get_query(sp);
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let new_query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");
    sp_set_query(sp, &new_query);

    Ok(JSValue::UNDEFINED)
}

fn js_sp_to_string(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    let query = sp_get_query(sp);
    let s = ctx.create_string_from_utf8(query.into_bytes())?;
    Ok(s.into())
}

fn js_sp_for_each(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let sp = get_sp(ctx, &this)?;
    if cc.length() == 0 {
        ctx.throw_type_error("forEach requires a callback");
        return Err(-1);
    }

    let cb = cc.arg(0).into_callable(ctx)?;
    let query = sp_get_query(sp);

    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        let val_js = JSValue::String(ctx.create_string_from_utf8(v.as_bytes().to_vec())?);
        let key_js = JSValue::String(ctx.create_string_from_utf8(k.as_bytes().to_vec())?);
        if cb
            .call(ctx, JSValue::UNDEFINED, &[val_js, key_js, this])
            .is_err()
        {
            return Err(-1);
        }
    }

    Ok(JSValue::UNDEFINED)
}

fn js_sp_entries(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error("URLSearchParams.entries() not yet implemented");
    Err(-1)
}

fn js_sp_keys(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error("URLSearchParams.keys() not yet implemented");
    Err(-1)
}

fn js_sp_values(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.throw_type_error("URLSearchParams.values() not yet implemented");
    Err(-1)
}

// ===========================================================================
// Registration: URL
// ===========================================================================

const CLASS_URL_PROTO: &[FieldDef] = &[
    FieldDef::GETSET(
        "href",
        Some(js_url_href_getter),
        Some(js_url_href_setter),
        0,
    ),
    FieldDef::GETSET("origin", Some(js_url_origin_getter), None, 0),
    FieldDef::GETSET(
        "protocol",
        Some(js_url_protocol_getter),
        Some(js_url_protocol_setter),
        0,
    ),
    FieldDef::GETSET(
        "username",
        Some(js_url_username_getter),
        Some(js_url_username_setter),
        0,
    ),
    FieldDef::GETSET(
        "password",
        Some(js_url_password_getter),
        Some(js_url_password_setter),
        0,
    ),
    FieldDef::GETSET(
        "host",
        Some(js_url_host_getter),
        Some(js_url_host_setter),
        0,
    ),
    FieldDef::GETSET(
        "hostname",
        Some(js_url_hostname_getter),
        Some(js_url_hostname_setter),
        0,
    ),
    FieldDef::GETSET(
        "port",
        Some(js_url_port_getter),
        Some(js_url_port_setter),
        0,
    ),
    FieldDef::GETSET(
        "pathname",
        Some(js_url_pathname_getter),
        Some(js_url_pathname_setter),
        0,
    ),
    FieldDef::GETSET(
        "search",
        Some(js_url_search_getter),
        Some(js_url_search_setter),
        0,
    ),
    FieldDef::GETSET(
        "hash",
        Some(js_url_hash_getter),
        Some(js_url_hash_setter),
        0,
    ),
    FieldDef::GETSET("searchParams", Some(js_url_search_params_getter), None, 0),
    FieldDef::FUNC("toString", 0, js_url_to_string, 0),
    FieldDef::FUNC("toJSON", 0, js_url_to_string, 0),
];

def_native_object!(
    "URL",
    JSURL,
    UrlPayload,
    CLASS_URL_PROTO,
    Some(ClassCtorDef::new(js_url_constructor, 2, true).expose_to_global())
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_url(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSURL::register(rt)
}

// ===========================================================================
// Registration: URLSearchParams
// ===========================================================================

const CLASS_SP_PROTO: &[FieldDef] = &[
    FieldDef::FUNC("append", 2, js_sp_append, 0),
    FieldDef::FUNC("delete", 1, js_sp_delete, 0),
    FieldDef::FUNC("get", 1, js_sp_get, 0),
    FieldDef::FUNC("getAll", 1, js_sp_get_all, 0),
    FieldDef::FUNC("has", 1, js_sp_has, 0),
    FieldDef::FUNC("set", 2, js_sp_set, 0),
    FieldDef::FUNC("sort", 0, js_sp_sort, 0),
    FieldDef::FUNC("toString", 0, js_sp_to_string, 0),
    FieldDef::FUNC("forEach", 1, js_sp_for_each, 0),
    FieldDef::FUNC("entries", 0, js_sp_entries, 0),
    FieldDef::FUNC("keys", 0, js_sp_keys, 0),
    FieldDef::FUNC("values", 0, js_sp_values, 0),
];

def_native_object!(
    "URLSearchParams",
    JSURLSearchParams,
    SearchParamsPayload,
    CLASS_SP_PROTO,
    Some(ClassCtorDef::new(js_sp_constructor, 1, true).expose_to_global())
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_url_search_params(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSURLSearchParams::register(rt)
}
