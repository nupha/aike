// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::RefCell;

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, Runtime, ScopedContext,
    def_native_object,
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::USER_CLASS_DEFS;

struct JSONLPayload {
    values: RefCell<Vec<JSValue>>,
}

impl JSONLPayload {
    fn gc_trace(&self, gcx: &mut GcTraceCtx) {
        if let Ok(values) = self.values.try_borrow() {
            for v in values.iter() {
                v.trace(gcx);
            }
        }
    }
}

const CLASS_JSONLARRAY_PROTO: &[FieldDef] = &[
    FieldDef::GETSET("length", Some(js_length), None, 0),
    FieldDef::FUNC("get", 1, js_get, 0),
    FieldDef::FUNC("push", 0, js_push, 0),
    FieldDef::FUNC("splice", 0, js_splice, 0),
    FieldDef::FUNC("slice", 0, js_slice, 0),
    FieldDef::FUNC("[Symbol.iterator]", 0, js_iterator, 0),
    FieldDef::FUNC("stringify", 0, js_stringify, 0),
    FieldDef::FUNC("toArray", 0, js_to_array, 0),
];

/// Constructor: `new JSONLArray(values?)` — copy elements of an optional array.
fn js_jsonl_array_ctor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut values: Vec<JSValue> = Vec::new();
    if cc.length() > 0 {
        let arg = cc.arg(0);
        if let Some(obj) = arg.as_object() {
            if let Ok(len) = obj.get_property_str(ctx, "length")?.to_number(ctx) {
                let n = len.to_i32();
                for i in 0..n {
                    values.push(obj.get_property_str(ctx, &i.to_string())?);
                }
            }
        }
    }
    JSJSONLArray::new_instance(
        ctx,
        cc.this(),
        JSONLPayload {
            values: RefCell::new(values),
        },
    )
    .map(JSValue::from)
}

const JSJSONLARRAY_CTOR: ClassCtorDef = ClassCtorDef::new(js_jsonl_array_ctor, 1, true);

def_native_object!(
    "JSONLArray",
    JSJSONLArray,
    JSONLPayload,
    CLASS_JSONLARRAY_PROTO,
    Some(JSJSONLARRAY_CTOR),
    on_payload_gc: JSONLPayload::gc_trace
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_jsonl_array(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSJSONLArray::register(rt)
}

fn get_payload<'a>(cx: &mut ScopedContext, val: &'a JSValue) -> Result<&'a JSONLPayload, i32> {
    match val
        .as_native_object::<JSJSONLArray>()
        .and_then(|o| o.payload())
    {
        Some(p) => Ok(p),
        None => {
            cx.throw_type_error("not a JSONLArray");
            Err(-1)
        }
    }
}

fn get_payload_mut<'a>(
    cx: &mut ScopedContext,
    val: &'a mut JSValue,
) -> Result<&'a mut JSONLPayload, i32> {
    match val
        .as_native_object_mut::<JSJSONLArray>()
        .and_then(|o| o.payload_mut())
    {
        Some(p) => Ok(p),
        None => {
            cx.throw_type_error("not a JSONLArray");
            Err(-1)
        }
    }
}

/// Build a JSONLArray instance from a set of JS values.
pub(crate) fn make_jsonl_array(
    cx: &mut ScopedContext,
    values: Vec<JSValue>,
) -> Result<JSValue, i32> {
    JSJSONLArray::new_instance(
        cx,
        JSValue::UNDEFINED,
        JSONLPayload {
            values: RefCell::new(values),
        },
    )
    .map(JSValue::from)
}

/// Serialize a JSONLArray instance to a JSONL string.
pub(crate) fn jsonl_array_stringify(
    cx: &mut ScopedContext,
    arr: &JSJSONLArray,
) -> Result<String, i32> {
    let p = arr.payload().ok_or_else(|| {
        cx.throw_type_error("not a JSONLArray");
        -1
    })?;
    jsonl_stringify_from_values(cx, &p.values.borrow())
}

/// Serialize values to a JSONL string (one JSON object per line).
pub(crate) fn jsonl_stringify_from_values(
    cx: &mut ScopedContext,
    values: &[JSValue],
) -> Result<String, i32> {
    let mut out = String::new();
    for v in values {
        let json = v.to_serde_json(cx)?;
        let line = serde_json::to_string(&json).map_err(|e| {
            cx.throw_internal_error(&e.to_string());
            -1
        })?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

// ── Instance methods ─────────────────────────────────────────────────────────

fn js_length(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let arr = get_payload(cx, &this)?;
    Ok(JSValue::from(arr.values.borrow().len() as i32))
}

fn js_get(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let arr = get_payload(cx, &this)?;
    let values = arr.values.borrow();
    let len = values.len() as i64;
    let mut i = cc.arg(0).to_number(cx)?.to_i32() as i64;
    if i < 0 {
        i += len;
    }
    if i < 0 || i >= len {
        if cc.length() > 1 && !cc.arg(1).is_undefined() {
            return Ok(cc.arg(1));
        }
        return Ok(JSValue::UNDEFINED);
    }
    Ok(values[i as usize])
}

fn js_push(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let arr = get_payload_mut(cx, &mut this)?;
    let mut values = arr.values.borrow_mut();
    for a in cc.arguments() {
        values.push(*a);
    }
    Ok(JSValue::from(values.len() as i32))
}

fn js_splice(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut this = cc.this();
    let arr = get_payload_mut(cx, &mut this)?;
    let mut values = arr.values.borrow_mut();
    let len = values.len() as i64;

    let start_raw = if cc.length() > 0 {
        cc.arg(0).to_number(cx)?.to_i32() as i64
    } else {
        0
    };
    let mut start = if start_raw < 0 {
        len + start_raw
    } else {
        start_raw
    };
    if start < 0 {
        start = 0;
    }
    if start > len {
        start = len;
    }

    let delete_count = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        let d = cc.arg(1).to_number(cx)?.to_i32() as i64;
        if d < 0 { 0 } else { d }
    } else {
        len - start
    };

    let items: Vec<JSValue> = (2..cc.length()).map(|i| cc.arg(i)).collect();

    let start_usize = start as usize;
    let delete_usize = (delete_count.min(len - start)).max(0) as usize;
    let removed: Vec<JSValue> = values
        .drain(start_usize..start_usize + delete_usize)
        .collect();
    for (off, item) in items.into_iter().enumerate() {
        values.insert(start_usize + off, item);
    }
    drop(values);

    make_jsonl_array(cx, removed)
}

fn js_slice(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let arr = get_payload(cx, &this)?;
    let values = arr.values.borrow();
    let len = values.len() as i64;

    let mut start = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(cx)?.to_i32() as i64
    } else {
        0
    };
    if start < 0 {
        start = len + start;
    }
    if start < 0 {
        start = 0;
    }

    let mut end = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        cc.arg(1).to_number(cx)?.to_i32() as i64
    } else {
        len
    };
    if end < 0 {
        end = len + end;
    }
    if end > len {
        end = len;
    }
    if start > end {
        start = end;
    }

    let slice: Vec<JSValue> = values[start as usize..end as usize].to_vec();
    make_jsonl_array(cx, slice)
}

fn js_iterator(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let arr = get_payload(cx, &this)?;
    let arr_obj = Array::from_values(cx, arr.values.borrow().clone())?;
    let iter_fn = arr_obj.get_property_str(cx, "[Symbol.iterator]")?;
    let cb = iter_fn.into_callable(cx)?;
    cb.call(cx, arr_obj.into(), &[])
}

fn js_stringify(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let arr = get_payload(cx, &this)?;
    let s = jsonl_stringify_from_values(cx, &arr.values.borrow())?;
    cx.create_string_from_utf8(s.into_bytes())
        .map(JSValue::from)
}

/// `toArray()` — return a plain array copy of the JSONLArray elements.
fn js_to_array(cx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let arr = get_payload(cx, &this)?;
    Array::from_values(cx, arr.values.borrow().clone()).map(JSValue::from)
}
