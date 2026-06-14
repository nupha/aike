// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::path::{Component, Path, PathBuf};

use zuqe::{CallContext, FieldDef, JSResult, JSValue, ScopedContext};

/// Normalize a path by resolving `.` and `..` components without following symlinks.
fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    let mut rooted = false;

    for component in path.components() {
        match component {
            Component::Prefix(p) => {
                result.push(p.as_os_str());
            }
            Component::RootDir => {
                if !rooted {
                    result.push(component.as_os_str());
                    rooted = true;
                }
            }
            Component::Normal(name) => result.push(name),
            Component::ParentDir => {
                if !result.as_os_str().is_empty() {
                    result.pop();
                }
            }
            Component::CurDir => {}
        }
    }
    result
}

/// Helper: extract Rust String from a JS argument.
fn arg_to_string(ctx: &mut ScopedContext, cc: &CallContext, idx: usize) -> Result<String, i32> {
    let s = cc.arg(idx).to_string(ctx)?;
    Ok(s.content().encode_utf8(false).to_string())
}

/// Helper: create a JS string from a Rust &str.
fn js_str(ctx: &mut ScopedContext, s: &str) -> Result<JSValue, i32> {
    ctx.create_string_from_utf8(s.as_bytes().to_vec())
        .map(JSValue::from)
}

// magic constants for js_path_extract
const EXTRACT_BASENAME: i32 = 0;
const EXTRACT_DIRNAME: i32 = 1;
const EXTRACT_EXTNAME: i32 = 2;

/// Shared handler for basename / dirname / extname.
/// Selected by `magic`: 0=basename, 1=dirname, 2=extname.
fn js_path_extract(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let p = arg_to_string(ctx, &cc, 0)?;
    let path = Path::new(&p);

    let result = match cc.magic() {
        EXTRACT_BASENAME => {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Optional ext suffix removal
            if cc.length() > 1 {
                let ext = arg_to_string(ctx, &cc, 1)?;
                if !ext.is_empty() && base.ends_with(&ext) && base.len() > ext.len() {
                    base[..base.len() - ext.len()].to_string()
                } else {
                    base
                }
            } else {
                base
            }
        }
        EXTRACT_DIRNAME => path
            .parent()
            .map(|d| {
                let s = d.to_string_lossy();
                if s.is_empty() {
                    ".".to_string()
                } else {
                    s.into_owned()
                }
            })
            .unwrap_or_else(|| ".".to_string()),
        EXTRACT_EXTNAME => path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default(),
        _ => unreachable!(),
    };

    js_str(ctx, &result)
}

// magic constants for js_path_combine
const COMBINE_JOIN: i32 = 0;
const COMBINE_RESOLVE: i32 = 1;

/// Shared handler for join / resolve.
/// Selected by `magic`: 0=join, 1=resolve.
fn js_path_combine(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let argc = cc.length();

    let buf = if cc.magic() == COMBINE_JOIN {
        // join: accumulate left to right
        let mut buf = PathBuf::new();
        for i in 0..argc {
            let seg = arg_to_string(ctx, &cc, i)?;
            buf.push(&seg);
        }
        buf
    } else {
        // resolve: right to left, stop at first absolute segment
        let mut buf = PathBuf::new();
        let mut absolute = false;
        for i in (0..argc).rev() {
            let seg = arg_to_string(ctx, &cc, i)?;
            let p = Path::new(&seg);
            if p.is_absolute() {
                buf = p.to_path_buf().join(&buf);
                absolute = true;
                break;
            }
            buf = p.to_path_buf().join(&buf);
        }
        if !absolute {
            let cwd = std::env::current_dir().map_err(|e| {
                ctx.throw_internal_error(&e.to_string());
                -1
            })?;
            buf = cwd.join(&buf);
        }
        buf
    };

    let result = normalize_path(&buf);
    let s = result.to_string_lossy();
    js_str(ctx, &s)
}

// ---------------------------------------------------------------------------
// path.parse(p)  →  { root, dir, base, name, ext }
// ---------------------------------------------------------------------------
fn js_path_parse(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let p = arg_to_string(ctx, &cc, 0)?;
    let path = Path::new(&p);

    let root = path
        .components()
        .find_map(|c| match c {
            Component::RootDir | Component::Prefix(_) => {
                Some(c.as_os_str().to_string_lossy().into_owned())
            }
            _ => None,
        })
        .unwrap_or_default();

    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();

    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let name = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    // Build JS values first (avoids double-mutable-borrow on ctx)
    let v_root = js_str(ctx, &root)?;
    let v_dir = js_str(ctx, &dir)?;
    let v_base = js_str(ctx, &base)?;
    let v_name = js_str(ctx, &name)?;
    let v_ext = js_str(ctx, &ext)?;

    let mut obj = ctx.create_base_object()?;
    obj.define_property_value_str(ctx, "root", v_root, zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "dir", v_dir, zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "base", v_base, zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "name", v_name, zuqe::JS_PROP_ENUMERABLE)?;
    obj.define_property_value_str(ctx, "ext", v_ext, zuqe::JS_PROP_ENUMERABLE)?;

    Ok(obj.into())
}

// ---------------------------------------------------------------------------
// path.format(pathObj)
// ---------------------------------------------------------------------------
fn js_path_format(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut obj = cc.arg(0).clone();
    let obj_ref = obj.as_object_mut().ok_or_else(|| {
        ctx.throw_type_error("path.format: argument must be an object");
        -1
    })?;

    // Read fields (treat undefined/null as empty string)
    let read_str = |ctx: &mut ScopedContext, obj: &mut zuqe::Object, key: &str| -> String {
        obj.get_property_str(ctx, key)
            .ok()
            .filter(|v| !v.is_undefined_or_null())
            .and_then(|v| v.to_string(ctx).ok())
            .map(|s| s.content().encode_utf8(false).to_string())
            .unwrap_or_default()
    };

    let dir = read_str(ctx, obj_ref, "dir");
    let root = read_str(ctx, obj_ref, "root");
    let base = read_str(ctx, obj_ref, "base");
    let name = read_str(ctx, obj_ref, "name");
    let ext = read_str(ctx, obj_ref, "ext");

    // Node.js format rules:
    // 1. If `dir` is provided, use dir + sep + base (or name+ext if no base)
    // 2. If `root` is provided (no dir), use root + base (or name+ext)
    // 3. Otherwise just base (or name+ext)
    let tail = if !base.is_empty() {
        base
    } else {
        format!("{name}{ext}")
    };

    let result = if !dir.is_empty() {
        if dir.ends_with('/') || dir.ends_with('\\') {
            format!("{dir}{tail}")
        } else {
            format!("{dir}{}{tail}", std::path::MAIN_SEPARATOR)
        }
    } else if !root.is_empty() {
        if root.ends_with('/') || root.ends_with('\\') {
            format!("{root}{tail}")
        } else {
            format!("{root}{}{tail}", std::path::MAIN_SEPARATOR)
        }
    } else {
        tail
    };

    js_str(ctx, &result)
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------
const MOD_PATH_ENTRIES: &[FieldDef] = &[
    FieldDef::FUNC("basename", 2, js_path_extract, EXTRACT_BASENAME as _),
    FieldDef::FUNC("dirname", 1, js_path_extract, EXTRACT_DIRNAME as _),
    FieldDef::FUNC("extname", 1, js_path_extract, EXTRACT_EXTNAME as _),
    FieldDef::FUNC("join", 1, js_path_combine, COMBINE_JOIN as _),
    FieldDef::FUNC("resolve", 1, js_path_combine, COMBINE_RESOLVE as _),
    FieldDef::FUNC("parse", 1, js_path_parse, 0),
    FieldDef::FUNC("format", 1, js_path_format, 0),
];

pub fn init_mod_path(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("path", MOD_PATH_ENTRIES)?;
    Ok(())
}
