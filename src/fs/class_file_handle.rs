// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ffi::c_void, path::Path};

use linkme::distributed_slice;
use zuqe::{
    CallContext, ClassDef, ClassID, FieldDef, JSResult, JSValue, Object, PollSender, Runtime,
    RuntimeImpl, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::USER_CLASS_DEFS;
use crate::fs::strip_chroot;

// Magic values for GETSET properties
const PROP_PATH: i32 = 0;
const PROP_NAME: i32 = 1;
const PROP_TYPE: i32 = 2;
const PROP_SIZE: i32 = 3;
const PROP_LAST_MODIFIED: i32 = 4;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
fn get_path_from_obj(
    ctx: &mut ScopedContext,
    this: &JSValue,
    class_id: ClassID,
) -> Result<String, i32> {
    let obj = this.retrive_object_payload(ctx, class_id)?;
    let opaque = obj.other().ok_or_else(|| -1)?.opaque();
    if opaque.is_null() {
        return Ok(String::new());
    }
    let s: &String = unsafe { &*opaque.cast::<String>() };
    Ok(s.clone())
}

/// Public helper used by mod.rs write() to extract path from a FileHandle JSValue.
pub fn handle_get_path(ctx: &mut ScopedContext, val: &JSValue) -> String {
    get_path_from_obj(ctx, val, JSFileHandle::class_id()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Getter: path, name, type, size, lastModified
// ---------------------------------------------------------------------------
fn js_fh_getter(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let class_id = JSFileHandle::class_id();
    let path = get_path_from_obj(ctx, &this, class_id)?;

    match cc.magic() {
        PROP_PATH => {
            let stripped = strip_chroot(&path);
            ctx.create_string_from_utf8(stripped.into_bytes())
                .map(JSValue::from)
        }
        PROP_NAME => {
            let name = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            ctx.create_string_from_utf8(name.into_bytes())
                .map(JSValue::from)
        }
        PROP_TYPE => {
            let ext = Path::new(&path)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            ctx.create_string_from_utf8(ext.into_bytes())
                .map(JSValue::from)
        }
        PROP_SIZE => {
            // synchronously stat for getter
            match std::fs::metadata(&path) {
                Ok(m) => Ok(JSValue::from(m.len() as i64)),
                Err(_) => Ok(JSValue::from(0i64)),
            }
        }
        PROP_LAST_MODIFIED => match std::fs::metadata(&path) {
            Ok(m) => {
                let ms = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0);
                Ok(JSValue::from(ms))
            }
            Err(_) => Ok(JSValue::from(0f64)),
        },
        _ => Ok(JSValue::UNDEFINED),
    }
}

// ---------------------------------------------------------------------------
// Instance methods (all async via Promise + tokio)
// ---------------------------------------------------------------------------
fn js_fh_text(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let path = get_path_from_obj(ctx, &this, JSFileHandle::class_id())?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<String, String>>(ctx, |cx, payload| -> JSResult {
            let text = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            cx.create_string_from_utf8(text.into_bytes())
                .map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::read_to_string(&path)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

fn js_fh_json(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let path = get_path_from_obj(ctx, &this, JSFileHandle::class_id())?;

    let (promise, tx) = Promise::create_with_channel::<Result<serde_json::Value, String>>(
        ctx,
        |cx, payload| -> JSResult {
            let val = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            JSValue::from_serde_json(cx, &val)
        },
    )?;

    tokio::spawn(async move {
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| e.to_string());
        let result = match raw {
            Ok(s) => serde_json::from_str(&s).map_err(|e| e.to_string()),
            Err(e) => Err(e),
        };

        let _ = PollSender::from(tx).send_async(result).await;
    });

    Ok(promise.into())
}

fn js_fh_bytes(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let path = get_path_from_obj(ctx, &this, JSFileHandle::class_id())?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, payload| -> JSResult {
            let bytes = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            TypedArray::create_from_vec_u8(cx, bytes).map(JSValue::from)
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(tokio::fs::read(&path).await.map_err(|e| e.to_string()))
            .await;
    });

    Ok(promise.into())
}

fn js_fh_exists(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let path = get_path_from_obj(ctx, &this, JSFileHandle::class_id())?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<bool, String>>(ctx, |cx, payload| -> JSResult {
            let ok = payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::Bool(ok))
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::try_exists(&path)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

fn js_fh_delete(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let path = get_path_from_obj(ctx, &this, JSFileHandle::class_id())?;

    let (promise, tx) =
        Promise::create_with_channel::<Result<(), String>>(ctx, |cx, payload| -> JSResult {
            payload.map_err(|e| {
                cx.throw_internal_error(&e);
                -1
            })?;
            Ok(JSValue::UNDEFINED)
        })?;

    tokio::spawn(async move {
        let _ = PollSender::from(tx)
            .send_async(
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| e.to_string()),
            )
            .await;
    });

    Ok(promise.into())
}

fn js_fh_stream(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    // stream() is not yet implemented for MVP.
    Ok(JSValue::NULL)
}

// ---------------------------------------------------------------------------
// Class definition
// ---------------------------------------------------------------------------
const CLASS_FILE_HANDLE_PROTO: &[FieldDef] = &[
    // readonly getter properties
    FieldDef::GETSET("path", Some(js_fh_getter), None, PROP_PATH as _),
    FieldDef::GETSET("name", Some(js_fh_getter), None, PROP_NAME as _),
    FieldDef::GETSET("type", Some(js_fh_getter), None, PROP_TYPE as _),
    FieldDef::GETSET("size", Some(js_fh_getter), None, PROP_SIZE as _),
    FieldDef::GETSET(
        "lastModified",
        Some(js_fh_getter),
        None,
        PROP_LAST_MODIFIED as _,
    ),
    // async methods
    FieldDef::CALL("text", 0, js_fh_text, 0),
    FieldDef::CALL("json", 0, js_fh_json, 0),
    FieldDef::CALL("bytes", 0, js_fh_bytes, 0),
    FieldDef::CALL("stream", 0, js_fh_stream, 0),
    FieldDef::CALL("exists", 0, js_fh_exists, 0),
    FieldDef::CALL("delete", 0, js_fh_delete, 0),
];

pub struct JSFileHandle;

impl JSFileHandle {
    fn handle_dispose(_: &mut RuntimeImpl, opaque: *mut c_void, _: usize) {
        if !opaque.is_null() {
            let p = unsafe { Box::from_raw(opaque.cast::<String>()) };
            drop(p);
        }
    }

    thread_local! {
        static CLASS_ID: Cell<ClassID> = const { Cell::new(0) };
    }

    fn register(rt: &mut Runtime) -> Result<ClassID, i32> {
        let class_id = rt
            .register_class(
                ClassDef::new("FileHandle", CLASS_FILE_HANDLE_PROTO, None)
                    .on_dispose(Self::handle_dispose),
            )
            .expect("register class FileHandle failed");

        Self::CLASS_ID.with(|s| s.set(class_id));
        Ok(class_id)
    }

    #[inline(always)]
    pub fn class_id() -> ClassID {
        Self::CLASS_ID.with(|x| x.get())
    }

    pub fn new_instance(ctx: &mut ScopedContext, path: String) -> Result<Object, i32> {
        let class_id = Self::class_id();
        let opaque = Box::into_raw(Box::new(path)).cast::<c_void>();
        ctx.create_class_instance_with_opaque(class_id, opaque)
            .map_err(|e| {
                let p = e.other().unwrap();
                let _ = unsafe { Box::from_raw(p.opaque().cast::<String>()) };
                -1
            })
    }

    /// Check if a JSValue is a FileHandle instance (via payload retrieval).
    pub fn is_instance(ctx: &mut ScopedContext, val: &JSValue) -> bool {
        val.retrive_object_payload(ctx, Self::class_id()).is_ok()
    }
}

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_file_handle(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSFileHandle::register(rt)
}
