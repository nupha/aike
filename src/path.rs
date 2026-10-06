// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::path::{Component, MAIN_SEPARATOR, Path, PathBuf};

use url::Url;
use zuqe::{CallContext, FieldDef, JS_PROP_ENUMERABLE, JSResult, JSValue, ScopedContext};

use crate::whatwg::url::{new_url, url_arg_to_str};

/// A path broken into the pieces normalization needs: an optional drive, whether
/// it is rooted, and the ordinary segments after them.
struct Segments {
    drive: Option<std::ffi::OsString>,
    rooted: bool,
    /// `..` segments that could not be cancelled by a real name. They are counted
    /// rather than stored, because one must not cancel another: Node keeps
    /// `..\..\z` as `..\..\z`, where popping the first `..` against the second
    /// would leave just `z`.
    dots: usize,
    names: Vec<std::ffi::OsString>,
}

/// Split `path` into drive / root / segments, resolving `.` and `..` without
/// touching the filesystem.
///
/// Accumulating into a `PathBuf` does not work here. `PathBuf::push` discards the
/// buffer whenever the argument carries a prefix, and Rust reports an *interior*
/// `C:` as a prefix, so `join('C:\a', 'C:\a\..\..')` lost everything before it and
/// came out `C:` where Node says `C:\a`. Node reads an interior drive as an
/// ordinary segment name, so the segments are kept as plain strings and only
/// reassembled once at the end.
fn split_normalized(path: &Path) -> Segments {
    let mut out = Segments {
        drive: None,
        rooted: false,
        dots: 0,
        names: Vec::new(),
    };

    for component in path.components() {
        match component {
            Component::Prefix(p) => {
                if out.drive.is_none() && !out.rooted && out.names.is_empty() {
                    out.drive = Some(p.as_os_str().to_os_string());
                } else {
                    out.names.push(p.as_os_str().to_os_string());
                }
            }
            Component::RootDir => {
                if !out.rooted {
                    out.rooted = true;
                }
            }
            Component::Normal(name) => out.names.push(name.to_os_string()),
            Component::ParentDir => {
                // Pop a real segment if there is one. Two cases must not pop.
                //
                // A rooted path has nothing above its root, so `/..` stays `/`.
                //
                // A relative path with nothing left to pop has to *keep* the `..`:
                // Node gives `path.join("..", "models")` === `..\models`. Dropping
                // it instead silently moves the path — `join("..", "models")` came
                // out as `models`, which resolves one directory too deep and made a
                // relative SPEECH_MODELS_DIR point at the wrong place entirely.
                //
                // The counter is separate from `names` on purpose: a pending `..`
                // must not cancel another pending `..`.
                if out.names.pop().is_none() && !out.rooted {
                    out.dots += 1;
                }
            }
            Component::CurDir => {}
        }
    }
    out
}

/// Reassemble segments into a path. `OsString::push` appends verbatim, so a
/// segment that happens to look like a drive (`C:`) stays a name here.
fn assemble(parts: &Segments) -> PathBuf {
    let mut s = std::ffi::OsString::new();
    if let Some(drive) = &parts.drive {
        s.push(drive);
    }
    // The root contributes a separator that the first segment must not double up
    // on, so whether one is owed is tracked rather than inferred from emptiness.
    let mut owed = parts.rooted;
    for _ in 0..parts.dots {
        if owed {
            s.push(MAIN_SEPARATOR.to_string());
        }
        s.push("..");
        owed = true;
    }
    for name in &parts.names {
        if owed {
            s.push(MAIN_SEPARATOR.to_string());
        }
        s.push(name);
        owed = true;
    }
    // A bare root still needs its separator: `C:\`, not `C:`.
    if parts.rooted && parts.dots == 0 && parts.names.is_empty() {
        s.push(MAIN_SEPARATOR.to_string());
    }
    // A drive with nothing after it names that drive's current directory, so Node
    // spells it `C:.`: `normalize('C:')` is `C:.`, not `C:`. Only an unrooted
    // drive — `C:x` is drive-relative, `C:\x` is not.
    if parts.drive.is_some() && !parts.rooted && parts.dots == 0 && parts.names.is_empty() {
        s.push(".");
    }
    PathBuf::from(s)
}

/// Normalize a path by resolving `.` and `..` components without following symlinks.
fn normalize_path(path: &Path) -> PathBuf {
    assemble(&split_normalized(path))
}

/// Helper: extract Rust String from a JS argument.
fn arg_to_string(ctx: &mut ScopedContext, cc: &CallContext, idx: usize) -> Result<String, i32> {
    let s = cc.arg(idx).to_string(ctx)?;
    Ok(s.content().encode_utf8(false).to_string())
}

/// Helper: create a JS string from a Rust &str.
fn js_str(ctx: &mut ScopedContext, s: &str) -> Result<JSValue, i32> {
    ctx.create_string_from_utf8(s.as_bytes().to_vec())
        .map(JSValue::make_string)
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

    let buf = match cc.magic() {
        COMBINE_JOIN => {
            // Node's `join` concatenates the arguments with the separator and
            // normalizes the result *once*. A later absolute-looking segment does
            // NOT reset what came before it — that is `resolve`, not `join`:
            // `join('C:\a', 'C:\a\..\..')` is `C:\a`, not `C:\`.
            //
            // Accumulating into a PathBuf cannot express that, because
            // `PathBuf::push` discards the buffer whenever the argument is
            // absolute. So the segments are joined as text first and normalized
            // afterwards. Rust parses an interior `C:` as an ordinary name, which
            // is what makes `C:\a\b\C:\c` come out right.
            let mut joined = String::new();
            for i in 0..argc {
                let seg = arg_to_string(ctx, &cc, i)?;
                // Node skips empty arguments outright rather than letting them
                // contribute a separator: `join('..', '')` is `..`, not `..\`.
                if seg.is_empty() {
                    continue;
                }
                if !joined.is_empty() {
                    joined.push(MAIN_SEPARATOR);
                }
                joined.push_str(&seg);
            }
            // Node keeps a trailing separator here too: `join('..\\')` is `..\`.
            // Noted on the joined text, because by the time it has been normalized
            // the information is gone.
            let had_trailing = ends_with_separator(&joined);
            let normalized = normalize_path(Path::new(&joined));
            let mut s = normalized.to_string_lossy().into_owned();
            if s.is_empty() {
                s.push('.');
                if had_trailing {
                    s.push(MAIN_SEPARATOR);
                }
            } else if had_trailing && !ends_with_separator(&s) {
                s.push(MAIN_SEPARATOR);
            }
            // Falls through to the shared normalize_path call below only for
            // `resolve`; return early for `join`.
            return js_str(ctx, &s);
        }
        COMBINE_RESOLVE => {
            // resolve: like Node's path.resolve — process segments right to left;
            // an absolute segment discards everything to its left. If the
            // accumulation is still relative, anchor it at the cwd so the result
            // is always an absolute path.
            let mut buf = PathBuf::new();
            for i in (0..argc).rev() {
                let seg = arg_to_string(ctx, &cc, i)?;
                buf = Path::new(&seg).to_path_buf().join(&buf);
                // `has_root`, not `is_absolute`: Rust's `is_absolute` also demands
                // a drive, so `/b` did not count as absolute and failed to discard
                // the arguments to its left. Node treats it as absolute and puts it
                // on the current drive.
                if has_root(Path::new(&seg)) {
                    break;
                }
            }
            if !buf.is_absolute() {
                // Anchor at the right base. A path carrying a drive but no root is
                // drive-relative and belongs to *that* drive's cwd, not the
                // process cwd: `resolve('C:x')` is `<cwd of C:>\x`. Joining the
                // buffer onto the cwd would not do, because the buffer's own drive
                // makes `PathBuf::push` clear the base again — which is how this
                // ended up answering the bare `C:x`.
                let parts = split_normalized(&buf);
                let base = match &parts.drive {
                    Some(drive) => drive_cwd(&drive.to_string_lossy()),
                    None => {
                        let cwd = std::env::current_dir()
                            .map_err(|e| ctx.throw_internal_error(&e.to_string()))?;
                        if !parts.rooted {
                            cwd
                        } else {
                            // Rooted with no drive: Node puts it on the *root* of the
                            // current drive, so `resolve('/x')` is `C:\x` rather than
                            // `<cwd>\x`.
                            let drive = cwd
                                .components()
                                .next()
                                .map(|c| c.as_os_str().to_os_string())
                                .unwrap_or_else(|| std::ffi::OsString::from("C:"));
                            PathBuf::from(format!("{}\\", drive.to_string_lossy()))
                        }
                    }
                };
                let tail = relative_tail(&parts);
                if tail.is_empty() {
                    buf = base;
                } else {
                    let mut text = base.to_string_lossy().into_owned();
                    if !ends_with_separator(&text) {
                        text.push(MAIN_SEPARATOR);
                    }
                    text.push_str(&tail.to_string_lossy());
                    buf = PathBuf::from(text);
                }
            }
            buf
        }
        _ => unreachable!(),
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
    let obj_ref = cc
        .arg(0)
        .as_object()
        .ok_or_else(|| ctx.throw_type_error("path.format: argument must be an object"))?;

    // Read fields (treat undefined/null as empty string)
    let read_str = |ctx: &mut ScopedContext, obj: zuqe::Object, key: &str| -> String {
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

// magic constants for js_path_inspect
const INSPECT_NORMALIZE: i32 = 0;
const INSPECT_IS_ABSOLUTE: i32 = 1;

// ---------------------------------------------------------------------------
// Shared handler for normalize / isAbsolute.
// Selected by `magic`: 0=normalize, 1=isAbsolute.
// ---------------------------------------------------------------------------
fn js_path_inspect(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let p = arg_to_string(ctx, &cc, 0)?;
    let path = Path::new(&p);
    match cc.magic() {
        INSPECT_NORMALIZE => {
            // Node keeps a trailing separator: `normalize('a\..\')` is `.\`, not
            // `.`. Whether the input had one is a property of the input alone, so
            // it is captured before normalization and re-applied after — unless the
            // result already ends in one (the root, `\`, is already just that).
            let had_trailing = ends_with_separator(&p);
            let result = normalize_path(path);
            let mut s = result.to_string_lossy().into_owned();
            if s.is_empty() {
                // Normalizing to nothing means "here". Node still honours a
                // trailing separator on that: `normalize('./')` is `.\`, not `.`.
                s.push('.');
                if had_trailing {
                    s.push(MAIN_SEPARATOR);
                }
            } else if had_trailing && !ends_with_separator(&s) {
                s.push(MAIN_SEPARATOR);
            }
            js_str(ctx, &s)
        }
        // Node's rule is "starts with a separator, or is drive+rooted", which is
        // exactly "has a root". `Path::is_absolute` is stricter on Windows — it
        // also demands a drive prefix, so it called `isAbsolute('/x')` false.
        INSPECT_IS_ABSOLUTE => Ok(JSValue::from(has_root(path))),
        _ => unreachable!(),
    }
}

/// Whether the string ends in a path separator, either flavour.
fn ends_with_separator(s: &str) -> bool {
    s.ends_with('/') || s.ends_with('\\')
}

/// Whether the path is rooted, with or without a drive prefix.
fn has_root(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::RootDir))
}

/// `path.resolve` for one argument: a rooted path stands on its own, anything else
/// is taken relative to the cwd.
fn resolve_from(cwd: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if has_root(p) {
        normalize_path(p)
    } else {
        normalize_path(&cwd.join(p))
    }
}

/// The current directory of `drive`, which is what a drive-relative path such as
/// `C:x` is measured from.
///
/// Windows keeps one current directory *per drive* and publishes it as a hidden
/// environment variable named `=C:`, which is where Node looks too. When that is
/// absent — a process that never visited `C:` in `cmd` — the process cwd stands in
/// if it is already on that drive, and otherwise the drive's root.
fn drive_cwd(drive: &str) -> PathBuf {
    let key = format!("={}", drive);
    if let Some(dir) = std::env::var_os(&key) {
        let p = PathBuf::from(dir);
        if has_root(&p) {
            return normalize_path(&p);
        }
    }
    if let Ok(cwd) = std::env::current_dir()
        && cwd.components().next() == Path::new(drive).components().next()
    {
        return cwd;
    }
    PathBuf::from(format!("{}\\", drive))
}

/// The part of a non-rooted path that has to be appended to a base directory:
/// pending `..` first, then the segments, which is the order they normalize to.
fn relative_tail(parts: &Segments) -> std::ffi::OsString {
    let mut tail = std::ffi::OsString::new();
    for i in 0..parts.dots {
        if i > 0 {
            tail.push(MAIN_SEPARATOR.to_string());
        }
        tail.push("..");
    }
    for (i, name) in parts.names.iter().enumerate() {
        if i > 0 || parts.dots > 0 {
            tail.push(MAIN_SEPARATOR.to_string());
        }
        tail.push(name);
    }
    tail
}

// ---------------------------------------------------------------------------
// path.relative(from, to)  →  string
// Returns the relative path from `from` to `to`.
// ---------------------------------------------------------------------------
fn js_path_relative(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let from_str = arg_to_string(ctx, &cc, 0)?;
    let to_str = arg_to_string(ctx, &cc, 1)?;

    // Node resolves both sides against the cwd before comparing them, so they are
    // always absolute and the common prefix is meaningful. Normalizing the raw
    // arguments instead made a relative `from` collapse against a relative `to`:
    // `relative('..', 'models')` came out `..\models` where Node says `models` is
    // *under* cwd, i.e. `aike\models`.
    let cwd = std::env::current_dir().map_err(|e| ctx.throw_internal_error(&e.to_string()))?;
    let from = split_normalized(&resolve_from(&cwd, &from_str));
    let to = split_normalized(&resolve_from(&cwd, &to_str));

    // Two named drives have nothing in common, so there is no relative path
    // between them and Node answers with the destination as-is. A missing drive
    // means "the current one", which is the same drive, so only compare when both
    // sides actually name one.
    if let (Some(a), Some(b)) = (&from.drive, &to.drive)
        && a != b
    {
        return js_str(ctx, &assemble(&to).to_string_lossy());
    }

    // Common prefix, counted in segments. Comparing `Component`s instead let the
    // destination's drive clear the `..` already emitted, because pushing a
    // prefixed component resets a `PathBuf`.
    let common = from
        .names
        .iter()
        .zip(to.names.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // Go up out of `from`, then down into `to`.
    let mut result = Segments {
        drive: None,
        rooted: false,
        dots: 0,
        names: Vec::new(),
    };
    for _ in common..from.names.len() {
        result.names.push(std::ffi::OsString::from(".."));
    }
    result.names.extend_from_slice(&to.names[common..]);

    let built = assemble(&result);
    let s = built.to_string_lossy();
    js_str(ctx, if s.is_empty() { "" } else { &s })
}

// ---------------------------------------------------------------------------
// path.fileURLToPath(url) / path.pathToFileURL(path)
// ---------------------------------------------------------------------------

/// `path.fileURLToPath(url)` — convert a `file:` URL (string or `URL` object)
/// to a platform path. Follows Node's semantics: non-file scheme, a non-empty
/// host on POSIX, percent-encoded separators, or a non-absolute path all throw.
fn js_file_url_to_path(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let text = url_arg_to_str(ctx, &cc.arg(0))?;
    let parsed =
        Url::parse(&text).map_err(|_| ctx.throw_type_error("`url` must be a valid URL"))?;
    if parsed.scheme() != "file" {
        return Err(ctx.throw_type_error("`url` must be a file: URL"));
    }

    // Reject percent-encoded separators: after decoding they are ambiguous with
    // real path separators, so Node refuses them (ERR_INVALID_FILE_URL_PATH).
    let lower = parsed.path().to_ascii_lowercase();
    if lower.contains("%2f") {
        return Err(ctx.throw_type_error("`url` must not contain an encoded '/'"));
    }
    if cfg!(windows) && lower.contains("%5c") {
        return Err(ctx.throw_type_error("`url` must not contain an encoded '\\'"));
    }
    // On POSIX a file URL must not carry a host; Windows uses it for UNC shares.
    if !cfg!(windows) {
        let host = parsed.host_str().unwrap_or("");
        if !host.is_empty() && host != "localhost" {
            return Err(ctx.throw_type_error("`url` host must be empty or 'localhost'"));
        }
    }

    let path = parsed
        .to_file_path()
        .map_err(|_| ctx.throw_type_error("`url` must be an absolute file: URL"))?;
    js_str(ctx, &path.to_string_lossy())
}

/// `path.pathToFileURL(path)` — convert a path to a `file:` `URL` object.
/// Resolves the path to absolute first (Node behavior), preserving a trailing
/// separator so directory URLs round-trip.
fn js_path_to_file_url(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let p = cc
        .arg(0)
        .as_string(false)
        .ok_or_else(|| ctx.throw_type_error("`path` must be a string"))?
        .content()
        .encode_utf8(false)
        .to_string();

    let is_dir = p.ends_with('/') || (cfg!(windows) && p.ends_with('\\'));
    let raw = Path::new(&p);
    let abs = if raw.is_absolute() {
        normalize_path(raw)
    } else {
        let cwd = std::env::current_dir().map_err(|e| ctx.throw_internal_error(&e.to_string()))?;
        normalize_path(&cwd.join(raw))
    };

    let parsed = if is_dir {
        Url::from_directory_path(&abs)
            .or_else(|_| Url::from_file_path(&abs))
            .map_err(|_| ctx.throw_type_error("`path` must be an absolute path"))?
    } else {
        Url::from_file_path(&abs)
            .map_err(|_| ctx.throw_type_error("`path` must be an absolute path"))?
    };

    new_url(ctx, parsed)
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
    FieldDef::FUNC("normalize", 1, js_path_inspect, INSPECT_NORMALIZE as _),
    FieldDef::FUNC("relative", 2, js_path_relative, 0),
    FieldDef::FUNC("isAbsolute", 1, js_path_inspect, INSPECT_IS_ABSOLUTE as _),
    FieldDef::FUNC("fileURLToPath", 1, js_file_url_to_path, 0),
    FieldDef::FUNC("pathToFileURL", 1, js_path_to_file_url, 0),
    // Platform path separator: POSIX "/" , Windows "\"
    FieldDef::LAZY_VALUE(
        "sep",
        |cx| {
            cx.create_string_from_utf8(std::path::MAIN_SEPARATOR_STR.as_bytes().to_vec())
                .map(JSValue::make_string)
        },
        JS_PROP_ENUMERABLE,
    ),
    // Environment variable delimiter: POSIX ":" , Windows ";"
    FieldDef::LAZY_VALUE(
        "delimiter",
        |cx| {
            let d = if cfg!(windows) { ";" } else { ":" };
            cx.create_string_from_utf8(d.as_bytes().to_vec())
                .map(JSValue::make_string)
        },
        JS_PROP_ENUMERABLE,
    ),
];

pub fn init_mod_path(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.create_module("path", MOD_PATH_ENTRIES)?;
    Ok(())
}
