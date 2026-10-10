// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! CommonJS module support.
//!
//! A CommonJS file is executed inside Node's module wrapper
//!
//! ```text
//! (function (exports, require, module, __filename, __dirname) { … })
//! ```
//!
//! and `require()` returns the resulting `module.exports`. The registry lives
//! in [`CjsState`] and is exposed to JS as `require.cache`, so deleting a key
//! makes the next `require()` run the file again — Node's semantics, obtained
//! for free by making the cache an ordinary JS object.
//!
//! Both interop directions are implemented:
//!
//! * `require()` of an ES module (Node >= 22.12) evaluates the module graph
//!   synchronously and returns its namespace object; a graph with top-level
//!   await raises `ERR_REQUIRE_ASYNC_MODULE`.
//! * `import` of a CommonJS file is served by generated ES source that calls
//!   `require()` and re-exports `default` plus the names found by the static
//!   scan in [`scan`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use url::Url;
use zuqe::{
    Array, CallContext, Callback, EvalFlags, EvalName, EvalType, Exception, JS_PROP_C_W_E,
    JS_PROP_WRITABLE, JSResult, JSValue, Object, ScopedContext,
    builtins::promise::{Promise, PromiseState},
    gc_lite::{GcTrace, GcTraceCtx},
};

use crate::{
    aike::AikeRuntimeState,
    module_loader::{module_search_dirs, read_script},
    transpile,
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Per-runtime CommonJS registry, reachable from JS as `require.cache` /
/// `require.extensions` / `require.main`.
///
/// All three live in [`AikeRuntimeState`] rather than in thread-locals so two
/// runtimes on one thread never share a registry.
pub(crate) struct CjsState {
    /// `require.cache` — resolved filename -> module record.
    cache: Option<JSValue>,
    /// `require.extensions` — extension -> `(module, filename) => void`.
    extensions: Option<JSValue>,
    /// `require.main` — the entry module record.
    main: Option<JSValue>,
    /// `require(esm)` results, keyed by target (a `file://` URL or a native
    /// module name). Node keeps required ES modules in the ES module registry
    /// rather than in `require.cache`; this map plays that role here.
    esm: HashMap<String, JSValue>,
    /// Scratch slot filled by the generated `require(esm)` wrapper module.
    captured: Option<JSValue>,
}

impl Default for CjsState {
    fn default() -> Self {
        Self {
            cache: None,
            extensions: None,
            main: None,
            esm: HashMap::new(),
            captured: None,
        }
    }
}

/// Every field here is a GC node reachable only from Rust, so the whole
/// registry must be traced or a collected module record would be used
/// after free. See `docs/gc.md`.
impl GcTrace for CjsState {
    fn trace(&self, gcx: &mut GcTraceCtx) {
        for v in [&self.cache, &self.extensions, &self.main, &self.captured] {
            v.trace(gcx);
        }
        for v in self.esm.values() {
            v.trace(gcx);
        }
    }
}

impl CjsState {
    /// `require.cache` — created on first use, then stable for the runtime's
    /// life so its identity survives across every `require` function.
    pub(crate) fn cache(ctx: &mut ScopedContext) -> Result<Object, i32> {
        if let Some(c) = state(ctx).cache {
            return Ok(c.as_object().expect("cache is an object"));
        }
        let obj = ctx.create_base_object()?;
        state(ctx).cache = Some(obj.into());
        Ok(obj)
    }

    /// `require.extensions` — created on first use.
    ///
    /// Only the two loaders aike actually implements are keyed, matching Node
    /// (`fs` there has a third, `.node`, for native addons). Everything else —
    /// `.cjs`, `.ts`, `.cts`, or a name the user registers — falls back to the
    /// `.js` loader, which is Node's own rule
    /// (`Module._extensions[ext] || Module._extensions['.js']`) and what makes
    /// `require.extensions['.js'] = …` also govern `.cjs` files.
    pub(crate) fn extensions(ctx: &mut ScopedContext) -> Result<Object, i32> {
        if let Some(e) = state(ctx).extensions {
            return Ok(e.as_object().expect("extensions is an object"));
        }
        let mut obj = ctx.create_base_object()?;
        for (ext, cb) in [(".js", js_load_js as _), (".json", js_load_json as _)] {
            let f = ctx.create_native_function(Callback::Func(cb), ext, 2, 0)?;
            obj.define_property_value_str(ctx, ext, f.into(), JS_PROP_C_W_E)?;
        }
        state(ctx).extensions = Some(obj.into());
        Ok(obj)
    }
}

/// Borrow this runtime's CommonJS registry.
fn state<'a, 'c>(ctx: &'a mut ScopedContext<'c>) -> &'a mut CjsState {
    &mut AikeRuntimeState::of(ctx).cjs
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn js_str(ctx: &mut ScopedContext, s: &str) -> Result<JSValue, i32> {
    ctx.create_string_from_utf8(s.as_bytes().to_vec())
        .map(JSValue::make_string)
}

/// Quote `s` as a JS string literal, for embedding in generated source.
fn js_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn parent_dir(file: &str) -> PathBuf {
    Path::new(file)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Directory a bare global `require()` resolves against: the running module's
/// own directory, or the CWD when there is none (Node's `-e` / REPL rule).
fn caller_dir(ctx: &mut ScopedContext) -> PathBuf {
    if let Some(name) = ctx.current_module_name() {
        let path = Url::parse(&name)
            .ok()
            .and_then(|u| zuqe::url_to_abspath(&u))
            .unwrap_or_else(|| PathBuf::from(&name));
        return parent_dir(&path_str(&path));
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Throw a Node-shaped `Error` carrying an `err.code` property.
///
/// Node tags its module errors with `err.code` on a plain `Error`, so the
/// object is built through the global `Error` constructor: `create_error_object`
/// has no plain-`Error` kind, and its closest one (`Eval`) would surface as
/// `EvalError`.
fn throw_code(ctx: &mut ScopedContext, code: &str, msg: &str) -> i32 {
    let Ok(err) = new_error(ctx, msg) else {
        return ctx.throw_internal_error(msg);
    };
    let mut obj = err.as_object().expect("Error() returns an object");
    if let Ok(v) = js_str(ctx, code) {
        let _ = obj.define_property_value_str(ctx, "code", v, JS_PROP_WRITABLE);
    }
    ctx.throw(Exception::Error(err));
    -1
}

/// `new Error(msg)` through the global constructor.
fn new_error(ctx: &mut ScopedContext, msg: &str) -> Result<JSValue, i32> {
    let ctor = ctx.global_obj().get_property_str(ctx, "Error")?;
    let ctor = ctor
        .as_object()
        .ok_or_else(|| ctx.throw_internal_error("Error is not callable"))?;
    let msg = js_str(ctx, msg)?;
    ctor.call(ctx, JSValue::UNDEFINED, &[msg])
}

/// `"Cannot find module '<id>'\nRequire stack:\n- /a.js\n- /b.js"`, walking
/// the `parent` chain the way Node does.
fn module_not_found(ctx: &mut ScopedContext, id: &str, from: Option<JSValue>) -> i32 {
    let mut msg = format!("Cannot find module '{id}'");
    let mut chain: Vec<String> = Vec::new();
    let mut cur = from.filter(|v| v.is_object());
    while let Some(v) = cur {
        let Some(o) = v.as_object() else { break };
        let Ok(file) = o.get_property_str(ctx, "filename") else {
            break;
        };
        let Some(s) = file.as_string(true) else { break };
        chain.push(s.content().encode_utf8(false).to_string());
        let Ok(parent) = o.get_property_str(ctx, "parent") else {
            break;
        };
        cur = Some(parent).filter(|p| p.is_object());
    }
    if !chain.is_empty() {
        msg.push_str("\nRequire stack:");
        for f in chain.iter().rev() {
            msg.push_str("\n- ");
            msg.push_str(f);
        }
    }
    throw_code(ctx, "MODULE_NOT_FOUND", &msg)
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// How a file must be evaluated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Format {
    Esm,
    Cjs,
    Json,
}

/// Decide a file's format from its extension, falling back to a scan of its
/// source for ES module syntax (Node's `DETECT_MODULE_SYNTAX`, restricted to
/// static `import`/`export` — aike's `ModuleDef::detect_module` only peeks at
/// the first token, which misjudges a file whose first statement is neither).
pub(crate) fn format_of(name: &str, source: &str) -> Format {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "mjs" | "mts" => return Format::Esm,
        "cjs" | "cts" => return Format::Cjs,
        "json" => return Format::Json,
        _ => {}
    }
    if scan::has_esm_syntax(source) {
        Format::Esm
    } else {
        Format::Cjs
    }
}

// ---------------------------------------------------------------------------
// require
// ---------------------------------------------------------------------------

/// magic for the shared `require` callback.
const REQ_LOAD: i16 = 0;
const REQ_RESOLVE: i16 = 1;
/// magic for `require.resolve.paths`.
const REQ_PATHS: i16 = 2;

/// Build a `require` function bound to the module at `filename`.
///
/// `module` is the owning module record; for the global `require` it is
/// `undefined`, and resolution then falls back to the running module's
/// directory (CWD when there is none).
fn make_require(
    ctx: &mut ScopedContext,
    filename: Option<&str>,
    module: Option<JSValue>,
) -> Result<JSValue, i32> {
    let data: Box<[JSValue]> = match filename {
        Some(f) => Box::from([js_str(ctx, f)?, module.unwrap_or(JSValue::UNDEFINED)]),
        None => Box::default(),
    };
    let req: JSValue = ctx
        .create_native_function_with_data(
            Callback::Func(js_require),
            "require",
            1,
            REQ_LOAD,
            data.clone(),
        )?
        .into();

    let cache = CjsState::cache(ctx)?;
    let exts = CjsState::extensions(ctx)?;
    let main = state(ctx).main.unwrap_or(JSValue::UNDEFINED);
    let resolve: JSValue = ctx
        .create_native_function_with_data(
            Callback::Func(js_require),
            "resolve",
            2,
            REQ_RESOLVE,
            data,
        )?
        .into();

    let mut req_obj = req.as_object().expect("require is a function object");
    req_obj.define_property_value_str(ctx, "cache", cache.into(), JS_PROP_C_W_E)?;
    req_obj.define_property_value_str(ctx, "extensions", exts.into(), JS_PROP_C_W_E)?;
    req_obj.define_property_value_str(ctx, "main", main, JS_PROP_C_W_E)?;

    // `require.resolve.paths` answers "where would this be looked up".
    let paths: JSValue = ctx
        .create_native_function(Callback::Func(js_require), "paths", 1, REQ_PATHS)?
        .into();
    resolve
        .as_object()
        .expect("resolve is a function object")
        .define_property_value_str(ctx, "paths", paths, JS_PROP_C_W_E)?;

    req_obj.define_property_value_str(ctx, "resolve", resolve, JS_PROP_C_W_E)?;

    Ok(req)
}

/// Install the global `require`.
///
/// CommonJS is not a module — it installs a global binding, so it belongs with
/// the other globals rather than among the `init_mod_*` registrars.
///
/// Node has no global `require`; aike evaluates scripts in global scope and
/// the generated ESM↔CJS interop source calls it, so it is installed as one.
/// It resolves against the running module's directory, or the CWD when there
/// is none — the rule Node's `module.createRequire` uses.
pub(crate) fn init_cjs(ctx: &mut ScopedContext) -> Result<(), i32> {
    // Created eagerly so `require.cache` / `require.extensions` are observable
    // (and identity-stable) before the first `require()`.
    CjsState::cache(ctx)?;
    CjsState::extensions(ctx)?;
    let req = make_require(ctx, None, None)?;
    ctx.global_obj()
        .define_property_value_str(ctx, "require", req, JS_PROP_C_W_E)?;
    Ok(())
}

/// Run the CommonJS entry module.
///
/// The source is passed in rather than re-read: the entry may be `-e` code,
/// which has no file behind it, and a file entry has already been transpiled
/// by the caller.
pub(crate) fn run_entry(ctx: &mut ScopedContext, filename: &str, source: &str) -> Result<(), i32> {
    let record = create_record(ctx, filename, None)?;

    // `-e` code has no file, so — as in Node — it is neither a `require.main`
    // nor a cache entry. (A `[…]` filename also cannot be a plain object key:
    // the engine reads such keys as well-known symbols.)
    let is_real_file = !(filename.starts_with('[') && filename.ends_with(']'));
    if is_real_file {
        // Node identifies the process entry as `.` rather than by filename.
        let dot = js_str(ctx, ".")?;
        record
            .as_object()
            .expect("record is an object")
            .set_property_str(ctx, "id", dot)?;

        state(ctx).main = Some(record);
        cache_insert(ctx, filename, record)?;

        // `require.main` was still unset while `create_record` built this
        // module's own `require`, so patch it here — `require.main === module`
        // must hold inside the entry body.
        for holder in [
            ctx.global_obj()
                .get_property_str(ctx, "require")?
                .as_object(),
            record
                .as_object()
                .and_then(|r| r.get_property_str(ctx, "require").ok())
                .and_then(|r| r.as_object()),
        ] {
            if let Some(mut req) = holder {
                req.set_property_str(ctx, "main", record)?;
            }
        }
    }

    let exports = record
        .as_object()
        .expect("record is an object")
        .get_property_str(ctx, "exports")?;
    run_wrapper(ctx, record, exports, filename, source)?;

    let mut rec = record.as_object().expect("record is an object");
    rec.set_property_str(ctx, "loaded", JSValue::TRUE)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// require() implementation
// ---------------------------------------------------------------------------

fn js_require(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let id_val = cc.arg(0);
    if !id_val.is_string() {
        return Err(ctx.throw_type_error("`id` must be a string"));
    }
    let mut id = id_val
        .as_string(true)
        .expect("checked above")
        .content()
        .encode_utf8(false)
        .to_string();
    // `node:` marks a builtin in Node; aike has none, but its native modules
    // answer to the bare name, so the prefix is simply dropped.
    if let Some(bare) = id.strip_prefix("node:") {
        id = bare.to_string();
    }

    let owner_file = cc
        .data(0)
        .as_string(true)
        .map(|s| s.content().encode_utf8(false).to_string());
    let owner = cc.data(1);
    let base = match &owner_file {
        Some(f) => parent_dir(f),
        None => caller_dir(ctx),
    };

    match cc.magic() {
        m if m == REQ_RESOLVE as i32 => match resolve(&id, &base) {
            Some(p) => js_str(ctx, &path_str(&p)),
            None => Err(module_not_found(ctx, &id, Some(owner))),
        },
        m if m == REQ_PATHS as i32 => {
            let dirs = module_search_dirs(Some(&base));
            let mut values = Vec::with_capacity(dirs.len());
            for d in dirs {
                values.push(js_str(ctx, &path_str(&d))?);
            }
            Ok(Array::from_values(ctx, values)?.into())
        }
        _ => {
            // Node resolves builtins before anything else; aike's native
            // modules (`fs`, `path`, …) play that role, so a bare name is
            // tried there first.
            if let Some(ns) = load_native(ctx, &id)? {
                return Ok(ns);
            }
            let Some(resolved) = resolve(&id, &base) else {
                return Err(module_not_found(ctx, &id, Some(owner)));
            };
            load(ctx, &path_str(&resolved), owner)
        }
    }
}

/// Load `file` and return its exports, honouring `require.cache`.
fn load(ctx: &mut ScopedContext, file: &str, parent: JSValue) -> Result<JSValue, i32> {
    if let Some(rec) = cache_lookup(ctx, file)? {
        return rec.get_property_str(ctx, "exports");
    }

    // `require()` of an ES module (Node >= 22.12) evaluates the module graph
    // synchronously and yields its namespace; it is not cached in
    // `require.cache`, matching Node.
    //
    // `.aike` is a native zapi extension: it has no CommonJS form at all, and
    // reading it as source would parse a dylib as JavaScript. It is reached
    // through the very same path as an `import`, so the engine's own
    // `load_zapi_extension` does the loading.
    let zapi = is_zapi(file);
    if zapi || {
        let (bytes, resolved) = read_script(file).map_err(|e| ctx.throw_reference_error(&e))?;
        let source = String::from_utf8_lossy(&bytes).into_owned();
        format_of(&resolved, &source) == Format::Esm
    } {
        let url = zuqe::path_to_url(file)
            .map_err(|e| ctx.throw_reference_error(&e.to_string()))?
            .to_string();
        return require_esm(ctx, &url)?
            .ok_or_else(|| ctx.throw_internal_error("module namespace was not captured"));
    }

    // Node falls back to the `.js` loader for an unregistered extension.
    let ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let exts = CjsState::extensions(ctx)?;
    let registered = exts.get_property_str(ctx, &ext)?;
    let handler = if registered.is_undefined() {
        exts.get_property_str(ctx, ".js")?
    } else {
        registered
    };
    let Some(handler) = handler.as_object() else {
        return Err(ctx.throw_type_error("require.extensions entry must be a function"));
    };

    // Registered before running so a cycle sees the partial `exports`, as in Node.
    let record = create_record(ctx, file, Some(parent))?;
    cache_insert(ctx, file, record)?;
    let filename_val = js_str(ctx, file)?;
    handler.call(ctx, JSValue::UNDEFINED, &[record, filename_val])?;

    let mut rec = record.as_object().expect("record is an object");
    rec.set_property_str(ctx, "loaded", JSValue::TRUE)?;
    rec.get_property_str(ctx, "exports")
}

fn cache_lookup(ctx: &mut ScopedContext, file: &str) -> Result<Option<JSValue>, i32> {
    let cache = CjsState::cache(ctx)?;
    Ok(cache
        .get_property_str(ctx, file)?
        .as_object()
        .map(Into::into))
}

fn cache_insert(ctx: &mut ScopedContext, file: &str, record: JSValue) -> Result<(), i32> {
    let mut cache = CjsState::cache(ctx)?;
    cache.set_property_str(ctx, file, record)?;
    Ok(())
}

/// Create a Node-shaped module record and link it into its parent's
/// `children`.
///
/// `parent` is `None` for a module with no CommonJS parent: `null` when it is
/// the process entry point, `undefined` when it came from somewhere that is
/// not a CommonJS module (an ES module, or `-e`) — Node's distinction.
fn create_record(
    ctx: &mut ScopedContext,
    file: &str,
    parent: Option<JSValue>,
) -> Result<JSValue, i32> {
    let mut m = ctx.create_base_object()?;
    let file_val = js_str(ctx, file)?;
    m.define_property_value_str(ctx, "id", file_val, JS_PROP_C_W_E)?;
    m.define_property_value_str(ctx, "filename", file_val, JS_PROP_C_W_E)?;

    let dir = parent_dir(file);
    let dir_val = js_str(ctx, &path_str(&dir))?;
    m.define_property_value_str(ctx, "path", dir_val, JS_PROP_C_W_E)?;
    // Node seeds `module.exports` with a fresh object; `exports` is a second
    // name for it until the body reassigns one of them.
    let exports = ctx.create_base_object()?;
    m.define_property_value_str(ctx, "exports", exports.into(), JS_PROP_C_W_E)?;
    m.define_property_value_str(ctx, "loaded", JSValue::FALSE, JS_PROP_C_W_E)?;
    // `null` for a file entry point (Node), `undefined` for one with no
    // CommonJS parent at all (`-e`, or a module reached from ESM).
    let no_parent = if file.starts_with('[') && file.ends_with(']') {
        JSValue::UNDEFINED
    } else {
        JSValue::NULL
    };
    m.define_property_value_str(
        ctx,
        "parent",
        parent.filter(|p| p.is_object()).unwrap_or(no_parent),
        JS_PROP_C_W_E,
    )?;
    let children = Array::create(ctx)?;
    m.define_property_value_str(ctx, "children", children.into(), JS_PROP_C_W_E)?;

    let dirs = module_search_dirs(Some(&dir));
    let mut values = Vec::with_capacity(dirs.len());
    for d in dirs {
        values.push(js_str(ctx, &path_str(&d))?);
    }
    let paths = Array::from_values(ctx, values)?;
    m.define_property_value_str(ctx, "paths", paths.into(), JS_PROP_C_W_E)?;

    let req = make_require(ctx, Some(file), Some(m.into()))?;
    m.define_property_value_str(ctx, "require", req, JS_PROP_C_W_E)?;

    let record = JSValue::from(m);
    // Node appends to `parent.children` the first time a module is loaded,
    // i.e. exactly here — `load` creates the record only on a cache miss.
    if let Some(mut kids) = parent
        .filter(|p| p.is_object())
        .and_then(|p| p.as_object())
        .and_then(|po| po.get_property_str(ctx, "children").ok())
        .and_then(|c| c.as_object())
    {
        let len = kids
            .get_property_str(ctx, "length")?
            .to_number(ctx)?
            .to_i32();
        kids.set_property_u32(ctx, len.max(0) as u32, record)?;
    }
    Ok(record)
}

// ---------------------------------------------------------------------------
// Extension loaders
// ---------------------------------------------------------------------------

/// `require.extensions['.js']` and friends: read the file and run it inside
/// Node's module wrapper.
fn js_load_js(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let module = cc.arg(0);
    let Some(rec) = module.as_object() else {
        return Err(ctx.throw_type_error("module must be an object"));
    };
    let file = rec
        .get_property_str(ctx, "filename")?
        .as_string(true)
        .expect("filename is a string")
        .content()
        .encode_utf8(false)
        .to_string();

    let (bytes, resolved) = read_script(&file).map_err(|e| ctx.throw_reference_error(&e))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let source = if transpile::needs_transpile(&resolved) {
        let r = transpile::transpile(&text).map_err(|e| {
            ctx.throw_syntax_error(&format!("failed to transpile '{resolved}': {e}"))
        })?;
        // `.cts` is CommonJS by definition, so only a plain `.ts` can turn
        // out to be an ES module here.
        if r.is_module && !resolved.ends_with(".cts") {
            return Err(throw_code(
                ctx,
                "ERR_REQUIRE_ESM",
                &format!("require() of ES Module {resolved} not supported."),
            ));
        }
        r.code
    } else {
        text
    };

    let exports = rec.get_property_str(ctx, "exports")?;
    run_wrapper(ctx, module, exports, &file, &source)?;
    Ok(JSValue::UNDEFINED)
}

/// Compile and invoke Node's module wrapper over `source`, with `this` bound
/// to `module.exports`.
///
/// The wrapper is evaluated under its own filename rather than being handed
/// to the engine as a module: that is what gives the body module-scoped `var`
/// declarations, and it keeps the host eval's base URL untouched while still
/// letting `current_module_name()` resolve relative `fs` / `fetch` specifiers
/// against this file.
fn run_wrapper(
    ctx: &mut ScopedContext,
    module: JSValue,
    exports: JSValue,
    file: &str,
    source: &str,
) -> Result<(), i32> {
    let wrapper =
        format!("(function (exports, require, module, __filename, __dirname) {{\n{source}\n}})");
    let url = zuqe::path_to_url(file)
        .map_err(|e| ctx.throw_reference_error(&e.to_string()))?
        .to_string();
    let func = ctx.eval(
        wrapper.as_bytes(),
        &EvalName::Name(url),
        EvalType::Global,
        EvalFlags::empty(),
    )?;
    let f = func
        .as_object()
        .ok_or_else(|| ctx.throw_internal_error("module wrapper did not compile"))?;

    let require = module
        .as_object()
        .expect("module is an object")
        .get_property_str(ctx, "require")?;
    let filename_val = js_str(ctx, file)?;
    let dirname_val = js_str(ctx, &path_str(&parent_dir(file)))?;
    f.call(
        ctx,
        exports,
        &[exports, require, module, filename_val, dirname_val],
    )?;
    Ok(())
}

/// `require.extensions['.json']`.
fn js_load_json(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let module = cc.arg(0);
    let Some(mut rec) = module.as_object() else {
        return Err(ctx.throw_type_error("module must be an object"));
    };
    let file = rec
        .get_property_str(ctx, "filename")?
        .as_string(true)
        .expect("filename is a string")
        .content()
        .encode_utf8(false)
        .to_string();
    let bytes = read_script(&file)
        .map_err(|e| ctx.throw_reference_error(&e))?
        .0;
    let text = String::from_utf8_lossy(&bytes);
    // Node strips a leading BOM before parsing JSON.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);

    let parse = ctx
        .global_obj()
        .get_property_str(ctx, "JSON")?
        .get_property_str(ctx, "parse")?;
    let text_val = js_str(ctx, text)?;
    let parsed = parse.as_object().expect("JSON.parse is a function").call(
        ctx,
        JSValue::UNDEFINED,
        &[text_val],
    )?;
    rec.set_property_str(ctx, "exports", parsed)?;
    Ok(JSValue::UNDEFINED)
}

// ---------------------------------------------------------------------------
// require(esm)
// ---------------------------------------------------------------------------

/// Global used by the generated `require(esm)` wrapper module to hand the
/// namespace back to Rust. Non-enumerable, so it stays out of `Object.keys`.
const CAPTURE: &str = "__AIKE_CJS_NS__";

fn install_capture(ctx: &mut ScopedContext) -> Result<(), i32> {
    let mut global = ctx.global_obj();
    if global.has_property_str(ctx, CAPTURE)? {
        return Ok(());
    }
    let f = ctx.create_native_function(Callback::Func(js_capture), CAPTURE, 1, 0)?;
    global.define_property_value_str(ctx, CAPTURE, f.into(), 0)?;
    Ok(())
}

fn js_capture(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    state(ctx).captured = Some(cc.arg(0));
    Ok(JSValue::UNDEFINED)
}

/// `require('fs')` — aike's native modules are registered under their bare
/// name and are reachable from ESM, so `require()` can hand back their
/// namespace too.
///
/// The name comes from the engine's own module registry rather than a list
/// maintained here: probing instead (by attempting an import and interpreting
/// the failure) would make a plain "no such module" indistinguishable from a
/// real error inside the module.
fn load_native(ctx: &mut ScopedContext, specifier: &str) -> Result<Option<JSValue>, i32> {
    // Already resolved once: skip the registry query entirely. `require` runs
    // ahead of `require.cache`, so without this every repeated `require('fs')`
    // — in a loop, or from a module loaded many times — would rebuild the list.
    if let Some(ns) = state(ctx).esm.get(specifier) {
        return Ok(Some(*ns));
    }
    // Native modules are registered under bare names, so a relative or
    // absolute specifier can never name one. This is the common case
    // (`require('./x.cjs')`) and keeps it off the registry path entirely.
    if !is_bare(specifier) {
        return Ok(None);
    }
    let natives = ctx.loaded_module_names(zuqe::ModuleNameFilter::NativeOnly);
    if !natives.iter().any(|n| n == specifier) {
        return Ok(None);
    }
    require_esm(ctx, specifier)
}

/// A native zapi extension, which is a shared library rather than source.
fn is_zapi(file: &str) -> bool {
    file.ends_with(".aike")
}

/// Node's bare-specifier test, as `resolve` applies it: only a name that is
/// neither relative nor absolute can reach a native module or the search path.
fn is_bare(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with("./")
        && !id.starts_with("../")
        && id != "."
        && id != ".."
        && !Path::new(id).is_absolute()
}

/// Evaluate `target` (a `file://` URL or a native module name) as an ES
/// module and return its namespace object.
///
/// `None` means "no such module": the lookup failure is a resolution error,
/// not an exception, so a missing native module falls through to the ordinary
/// "cannot find module" path instead of propagating.
fn require_esm(ctx: &mut ScopedContext, target: &str) -> Result<Option<JSValue>, i32> {
    if let Some(ns) = state(ctx).esm.get(target) {
        return Ok(Some(*ns));
    }
    install_capture(ctx)?;
    state(ctx).captured = None;

    // The generated wrapper exists only to surface the namespace, which the
    // engine's module evaluator resolves its promise with as `undefined`.
    // Importing the real specifier from inside it keeps the engine's own
    // module cache — and therefore single evaluation and `import.meta` —
    // in charge.
    let src = format!("import * as ns from {}; {CAPTURE}(ns);", js_literal(target));
    let promise = match ctx.eval(
        src.as_bytes(),
        &EvalName::BaseUrl(wrapper_url(target)),
        EvalType::Module,
        EvalFlags::empty(),
    ) {
        Ok(p) => p,
        Err(e) => {
            state(ctx).captured = None;
            return Err(e);
        }
    };

    let Ok(p) = Promise::try_from(promise) else {
        return Err(ctx.throw_internal_error("module evaluation did not return a promise"));
    };
    let ns = match p.state() {
        PromiseState::Fulfilled => state(ctx).captured.filter(|v| v.is_object()),
        PromiseState::Rejected => {
            let reason = p.result();
            state(ctx).captured = None;
            ctx.throw(Exception::Error(reason));
            return Err(-1);
        }
        PromiseState::Pending => {
            state(ctx).captured = None;
            return Err(throw_code(
                ctx,
                "ERR_REQUIRE_ASYNC_MODULE",
                "require() cannot be used on an ES Module that has top-level await. \
                 Please use dynamic import() instead.",
            ));
        }
    };
    let Some(ns) = ns else {
        return Err(ctx.throw_internal_error("module namespace was not captured"));
    };
    // Node marks a namespace that has a `default` export with `__esModule: true`
    // so transpiler-generated code recognises it. The marker is only meaningful
    // on a real object, and a module namespace is non-extensible, so the
    // marked value is a copy — exactly like Node, where the `require`d value is
    // then *not* identical to the `import()` namespace.
    let ns = mark_es_module(ctx, ns)?;
    state(ctx).esm.insert(target.to_string(), ns);
    Ok(Some(ns))
}

/// Node's `__esModule` marker: present only when the namespace exports a
/// `default` and does not already define the name. A namespace without a
/// `default` is returned untouched, so `require(x) === await import(x)` holds
/// there, as in Node.
fn mark_es_module(ctx: &mut ScopedContext, ns: JSValue) -> Result<JSValue, i32> {
    let Some(mut obj) = ns.as_object() else {
        return Ok(ns);
    };
    if !obj.has_own_property_str(ctx, "default")? || obj.has_own_property_str(ctx, "__esModule")? {
        return Ok(ns);
    }
    // Copy with a null prototype, matching a module namespace.
    let mut copy = ctx.create_base_object_with_proto(None)?;
    let names =
        obj.get_own_property_names(ctx, zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK)?;
    for name in names {
        let val = obj.get_property(ctx, name.atom)?;
        copy.define_property_value(ctx, name.atom, val, JS_PROP_C_W_E)?;
    }
    copy.define_property_value_str(ctx, "__esModule", JSValue::TRUE, JS_PROP_C_W_E)?;
    Ok(copy.into())
}

/// A `file://` URL that identifies a generated wrapper module and cannot
/// collide with a user file (the leading `__aike_cjs_require__` segment is not
/// a path a module loader would ever produce for real content).
fn wrapper_url(target: &str) -> Url {
    let mut out = String::from("file:///__aike_cjs_require__");
    for b in target.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    Url::parse(&out).expect("generated URL is well formed")
}

/// ES source that exposes a CommonJS file to an ES module, mirroring Node's
/// namespace: `default` is `module.exports`, plus every name the static scan
/// found. Values are read once, at evaluation time, exactly as Node copies
/// them onto the namespace.
pub(crate) fn esm_interop_source(file: &str, source: &str) -> String {
    let mut out = format!(
        "const __cjs = require({});\nexport default __cjs;\n",
        js_literal(file)
    );
    // Each name gets its own binding so that arbitrary export keys (`"a-b"`,
    // reserved words) stay legal, and a name that collides with a binding the
    // scanner also found (`const a = 1; exports.a = 2`) cannot clash either.
    for (i, name) in scan::exported_names(source).into_iter().enumerate() {
        // `default` is already `module.exports`, and `__esModule` is Node's
        // own marker — neither may be re-declared.
        if name == "default" || name == "__esModule" {
            continue;
        }
        out.push_str(&format!(
            "const __n{i} = __cjs[{}];\nexport {{ __n{i} as {} }};\n",
            js_literal(&name),
            js_literal(&name)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Resolution (LOAD_AS_FILE / LOAD_AS_DIRECTORY)
// ---------------------------------------------------------------------------

/// Extensions probed after an exact filename miss, in `require.extensions`
/// key order.
/// Extension probe order for `LOAD_AS_FILE` / `LOAD_AS_DIRECTORY`.
///
/// `.aike` (a native zapi extension) comes last so a JS/TS module of the same
/// name always wins, matching the order `module_loader::search_module` uses
/// for `import`.
const PROBE: [&str; 6] = [".js", ".json", ".cjs", ".ts", ".cts", ".aike"];

/// Node's `LOAD_AS_FILE` + `LOAD_AS_DIRECTORY`, with aike's search-path
/// policy standing in for `node_modules` lookup.
fn resolve(id: &str, base: &Path) -> Option<PathBuf> {
    if id.is_empty() {
        return None;
    }
    if Path::new(id).is_absolute() {
        return load_as_file(id).or_else(|| load_as_dir(id));
    }
    if id.starts_with("./") || id.starts_with("../") || id == "." || id == ".." {
        let joined = path_str(&base.join(id));
        return load_as_file(&joined).or_else(|| load_as_dir(&joined));
    }
    for dir in module_search_dirs(Some(base)) {
        let cand = path_str(&dir.join(id));
        if let Some(p) = load_as_file(&cand).or_else(|| load_as_dir(&cand)) {
            return Some(p);
        }
    }
    None
}

fn load_as_file(p: &str) -> Option<PathBuf> {
    let path = PathBuf::from(p);
    if path.is_file() {
        return canonical(path);
    }
    for ext in PROBE {
        let cand = PathBuf::from(format!("{p}{ext}"));
        if cand.is_file() {
            return canonical(cand);
        }
    }
    None
}

fn load_as_dir(p: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(p);
    if !dir.is_dir() {
        return None;
    }
    for ext in PROBE {
        let cand = dir.join(format!("index{ext}"));
        if cand.is_file() {
            return canonical(cand);
        }
    }
    None
}

/// Normalize a resolved file to the identity Node gives it: `.` / `..`
/// collapsed and symlinks followed, so `require.cache` keys, `__filename` and
/// `module.id` are all the same string whatever route reached the file.
/// Falls back to the input when canonicalization fails.
fn canonical(path: PathBuf) -> Option<PathBuf> {
    Some(crate::helpers::canonicalize_path(&path).unwrap_or(path))
}

// ---------------------------------------------------------------------------
// Static analysis
// ---------------------------------------------------------------------------

/// The subset of `cjs-module-lexer` aike implements: a small tokenizer
/// (strings, template literals, comments and regex literals skipped) plus the
/// re-export patterns Node recognises.
mod scan {
    #[derive(Debug, PartialEq, Clone)]
    enum Tok {
        Ident(String),
        Str(String),
        Punct(char),
        /// Numbers, regex literals, template chunks — value irrelevant here.
        Other,
    }

    /// Tokenize enough of `src` to pattern-match on top-level constructs.
    fn tokenize(src: &str) -> Vec<Tok> {
        let b: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        // Last significant token, used to tell a regex literal from division.
        let mut prev: Option<Tok> = None;
        while i < b.len() {
            let c = b[i];
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            if c == '/' && b.get(i + 1) == Some(&'/') {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            if c == '/' && b.get(i + 1) == Some(&'*') {
                i += 2;
                while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                continue;
            }
            if c == '"' || c == '\'' {
                let (s, next) = read_string(&b, i);
                out.push(Tok::Str(s));
                prev = Some(Tok::Str(String::new()));
                i = next;
                continue;
            }
            if c == '`' {
                let next = skip_template(&b, i);
                out.push(Tok::Other);
                prev = Some(Tok::Other);
                i = next;
                continue;
            }
            if c == '/' && regex_allowed(&prev) {
                let next = skip_regex(&b, i);
                out.push(Tok::Other);
                prev = Some(Tok::Other);
                i = next;
                continue;
            }
            if c.is_alphabetic() || c == '_' || c == '$' {
                let start = i;
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$') {
                    i += 1;
                }
                let t = Tok::Ident(b[start..i].iter().collect());
                prev = Some(t.clone());
                out.push(t);
                continue;
            }
            if c.is_ascii_digit() {
                while i < b.len() && (b[i].is_alphanumeric() || b[i] == '.' || b[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Other);
                prev = Some(Tok::Other);
                continue;
            }
            out.push(Tok::Punct(c));
            prev = Some(Tok::Punct(c));
            i += 1;
        }
        out
    }

    fn read_string(b: &[char], start: usize) -> (String, usize) {
        let quote = b[start];
        let mut s = String::new();
        let mut i = start + 1;
        while i < b.len() {
            let c = b[i];
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == quote {
                return (s, i + 1);
            }
            s.push(c);
            i += 1;
        }
        (s, i)
    }

    /// Skip a template literal, descending into `${ … }` substitutions (which
    /// may themselves contain templates).
    fn skip_template(b: &[char], start: usize) -> usize {
        let mut i = start + 1;
        while i < b.len() {
            match b[i] {
                '\\' => i += 2,
                '`' => return i + 1,
                '$' if b.get(i + 1) == Some(&'{') => {
                    let mut depth = 1;
                    i += 2;
                    while i < b.len() && depth > 0 {
                        match b[i] {
                            '`' => i = skip_template(b, i),
                            '{' => {
                                depth += 1;
                                i += 1;
                            }
                            '}' => {
                                depth -= 1;
                                i += 1;
                            }
                            _ => i += 1,
                        }
                    }
                }
                _ => i += 1,
            }
        }
        i
    }

    fn skip_regex(b: &[char], start: usize) -> usize {
        let mut i = start + 1;
        let mut in_class = false;
        while i < b.len() {
            match b[i] {
                '\\' => i += 2,
                '[' => {
                    in_class = true;
                    i += 1;
                }
                ']' => {
                    in_class = false;
                    i += 1;
                }
                '/' if !in_class => return i + 1,
                '\n' => return i,
                _ => i += 1,
            }
        }
        i
    }

    /// A `/` starts a regex unless the previous token could end an operand.
    fn regex_allowed(prev: &Option<Tok>) -> bool {
        match prev {
            None => true,
            Some(Tok::Ident(_)) | Some(Tok::Str(_)) | Some(Tok::Other) => false,
            Some(Tok::Punct(c)) => !matches!(c, ')' | ']' | '}'),
        }
    }

    /// True when `src` contains ES module syntax: a static `import` or
    /// `export` at the top level. `import(` and `import.meta` are excluded,
    /// as are occurrences below the top level (e.g. inside a function).
    pub(super) fn has_esm_syntax(src: &str) -> bool {
        let toks = tokenize(src);
        let mut depth = 0i32;
        for (i, t) in toks.iter().enumerate() {
            match t {
                Tok::Punct(c) => match c {
                    '{' | '(' | '[' => depth += 1,
                    '}' | ')' | ']' => depth -= 1,
                    _ => {}
                },
                Tok::Ident(name) if depth == 0 => {
                    if name == "export" {
                        return true;
                    }
                    if name == "import" {
                        match toks.get(i + 1) {
                            Some(Tok::Punct('(')) | Some(Tok::Punct('.')) => {}
                            _ => return true,
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Names Node's `cjs-module-lexer` would report for `src`, sorted and
    /// deduplicated.
    pub(super) fn exported_names(src: &str) -> Vec<String> {
        let toks = tokenize(src);
        let mut names: Vec<String> = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            // exports.NAME =  /  module.exports.NAME =
            if let Some(rest) = exports_target(&toks, i) {
                if let Some(name) = member_name(&toks, rest) {
                    names.push(name);
                    i = rest + 2;
                    continue;
                }
                // exports['NAME'] =
                if matches!(toks.get(rest), Some(Tok::Punct('[')))
                    && let Some(Tok::Str(s)) = toks.get(rest + 1)
                    && matches!(toks.get(rest + 2), Some(Tok::Punct(']')))
                {
                    names.push(s.clone());
                    i = rest + 3;
                    continue;
                }
            }
            // Object.defineProperty(exports, 'NAME', { value: … })
            if let Some(name) = define_property_name(&toks, i) {
                names.push(name);
            }
            i += 1;
        }
        names.sort();
        names.dedup();
        names
    }

    /// If `toks[i]` starts `exports` or `module.exports`, return the index
    /// just past that target.
    fn exports_target(toks: &[Tok], i: usize) -> Option<usize> {
        match toks.get(i) {
            Some(Tok::Ident(n)) if n == "exports" => Some(i + 1),
            Some(Tok::Ident(n)) if n == "module" => match (toks.get(i + 1), toks.get(i + 2)) {
                (Some(Tok::Punct('.')), Some(Tok::Ident(e))) if e == "exports" => Some(i + 3),
                _ => None,
            },
            _ => None,
        }
    }

    /// `.NAME` at `i`, returning the identifier.
    fn member_name(toks: &[Tok], i: usize) -> Option<String> {
        if !matches!(toks.get(i), Some(Tok::Punct('.'))) {
            return None;
        }
        match toks.get(i + 1) {
            Some(Tok::Ident(n)) => Some(n.clone()),
            _ => None,
        }
    }

    /// `Object.defineProperty(<exports target>, 'NAME', { value: … })` —
    /// Node only reports the name when the descriptor object literal carries a
    /// `value` key; a getter-only descriptor is not reported.
    fn define_property_name(toks: &[Tok], i: usize) -> Option<String> {
        let is_ident = |k: usize, n: &str| matches!(toks.get(k), Some(Tok::Ident(x)) if x == n);
        let is_punct = |k: usize, c: char| matches!(toks.get(k), Some(Tok::Punct(x)) if *x == c);
        if !(is_ident(i, "Object")
            && is_punct(i + 1, '.')
            && is_ident(i + 2, "defineProperty")
            && is_punct(i + 3, '('))
        {
            return None;
        }
        let target = exports_target(toks, i + 4)?;
        if !is_punct(target, ',') {
            return None;
        }
        let Some(Tok::Str(name)) = toks.get(target + 1) else {
            return None;
        };
        if !is_punct(target + 2, ',') || !is_punct(target + 3, '{') {
            return None;
        }
        has_value_key(toks, target + 3).then(|| name.clone())
    }

    /// Does the object literal starting at `i` (`{`) contain a `value:` key
    /// at its own nesting level?
    fn has_value_key(toks: &[Tok], i: usize) -> bool {
        let mut depth = 0i32;
        let mut j = i;
        while j < toks.len() {
            match &toks[j] {
                Tok::Punct('{') | Tok::Punct('(') | Tok::Punct('[') => {
                    depth += 1;
                    j += 1;
                }
                Tok::Punct('}') | Tok::Punct(')') | Tok::Punct(']') => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                    j += 1;
                }
                Tok::Ident(n) if depth == 1 && n == "value" => {
                    if matches!(toks.get(j + 1), Some(Tok::Punct(':'))) {
                        return true;
                    }
                    j += 1;
                }
                _ => j += 1,
            }
        }
        false
    }
}

#[cfg(test)]
mod scan_tests {
    use super::scan::{exported_names, has_esm_syntax};

    #[test]
    fn detects_exports_assignments() {
        assert_eq!(exported_names("exports.a = 1;"), ["a"]);
        assert_eq!(exported_names("module.exports.b = 2;"), ["b"]);
        assert_eq!(exported_names("exports['c'] = 3;"), ["c"]);
        assert_eq!(exported_names("exports[\"d\"] = 4;"), ["d"]);
        assert_eq!(
            exported_names("function f(){ exports.inner = 1; }"),
            ["inner"]
        );
    }

    #[test]
    fn ignores_strings_and_comments() {
        let src = "const s = \"exports.fake = 1\";\n// exports.c = 2\n/* exports.b = 3 */\nexports.real = 4;";
        assert_eq!(exported_names(src), ["real"]);
    }

    #[test]
    fn ignores_templates_and_regex() {
        let src =
            "const t = `exports.inTemplate = 1`;\nconst r = /exports\\.inRegex/;\nexports.ok = 1;";
        assert_eq!(exported_names(src), ["ok"]);
    }

    #[test]
    fn define_property_needs_a_value_descriptor() {
        assert_eq!(
            exported_names("Object.defineProperty(exports, \"v\", { value: 1 });"),
            ["v"]
        );
        assert_eq!(
            exported_names("Object.defineProperty(module.exports, \"m\", { value: 1 });"),
            ["m"]
        );
        assert!(exported_names("Object.defineProperty(exports, \"g\", { get: f });").is_empty());
        assert!(
            exported_names("Object.defineProperties(exports, { d: { value: 1 } });").is_empty()
        );
    }

    #[test]
    fn deduplicates_and_sorts() {
        let src = "exports.b = 1; exports.a = 2; module.exports.b = 3;";
        assert_eq!(exported_names(src), ["a", "b"]);
    }

    #[test]
    fn detects_esm_syntax() {
        assert!(has_esm_syntax("import x from 'y';"));
        assert!(has_esm_syntax("export const a = 1;"));
        assert!(has_esm_syntax("const a = 1;\nexport { a };"));
        assert!(!has_esm_syntax("const a = require('b');"));
        assert!(!has_esm_syntax("import('x').then(m => m);"));
        assert!(!has_esm_syntax("obj.import.meta"));
        assert!(!has_esm_syntax("function f(){ const import = 1; }"));
        assert!(!has_esm_syntax("\"export const a = 1\";"));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod resolve_tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "aike_cjs_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            // `resolve` canonicalizes what it returns (symlinks collapsed), and
            // on macOS `temp_dir()` is `/var/...`, a symlink to `/private/var/...`.
            // Canonicalize here so expectations match what resolution yields.
            let p = crate::helpers::canonicalize_path(&p).unwrap_or(p);
            TempDir(p)
        }
        fn write(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
            p
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Resolution is pure filesystem work — no context involved.
    fn resolve_in(dir: &Path, id: &str) -> Option<PathBuf> {
        resolve(id, dir)
    }

    #[test]
    fn exact_file_wins_over_probed_extension() {
        let d = TempDir::new("exact");
        let exact = d.write("a.js");
        d.write("a.json");
        assert_eq!(resolve_in(d.path(), "./a.js"), Some(exact));
    }

    #[test]
    fn probes_extensions_in_order() {
        let d = TempDir::new("probe");
        d.write("a.js");
        assert_eq!(
            resolve_in(d.path(), "./a"),
            Some(d.0.join("a.js")),
            ".js is probed first"
        );
        std::fs::remove_file(d.0.join("a.js")).unwrap();
        d.write("a.json");
        assert_eq!(
            resolve_in(d.path(), "./a"),
            Some(d.0.join("a.json")),
            ".json is probed once .js is gone"
        );
    }

    #[test]
    fn resolves_directory_index() {
        let d = TempDir::new("dir");
        d.write("pkg/index.js");
        assert_eq!(
            resolve_in(d.path(), "./pkg"),
            Some(d.0.join("pkg/index.js"))
        );
    }

    #[test]
    fn absolute_and_parent_relative() {
        let d = TempDir::new("abs");
        let f = d.write("sub/x.js");
        assert_eq!(resolve_in(d.path(), &path_str(&f)), Some(f.clone()));
        assert_eq!(resolve_in(&d.0.join("sub"), "./x.js"), Some(f));
    }

    #[test]
    fn missing_and_empty_requests_fail() {
        let d = TempDir::new("miss");
        assert_eq!(resolve_in(d.path(), "./nope"), None);
        assert_eq!(resolve_in(d.path(), ""), None);
    }

    #[test]
    fn format_follows_extension_then_source() {
        assert_eq!(format_of("a.mjs", ""), Format::Esm);
        assert_eq!(format_of("a.cjs", "export const a = 1;"), Format::Cjs);
        assert_eq!(format_of("a.json", "{}"), Format::Json);
        assert_eq!(format_of("a.js", "export const a = 1;"), Format::Esm);
        assert_eq!(format_of("a.js", "module.exports = 1;"), Format::Cjs);
    }

    /// A zapi extension has no CommonJS form, so its extension — not its
    /// bytes — decides the path. Nothing may read a `.aike` file as source.
    #[test]
    fn zapi_extension_is_never_read_as_source() {
        assert!(is_zapi("x.aike"));
        assert!(!is_zapi("x.js"));
        assert!(!is_zapi("aike"));
    }

    /// `.aike` is probed last so a JS/TS module of the same name wins, the same
    /// order `module_loader::search_module` uses for `import`.
    #[test]
    fn js_wins_over_zapi_extension() {
        let d = TempDir::new("zapi");
        d.write("a.aike");
        assert_eq!(resolve_in(d.path(), "./a"), Some(d.0.join("a.aike")));
        d.write("a.js");
        assert_eq!(
            resolve_in(d.path(), "./a"),
            Some(d.0.join("a.js")),
            "a .js module takes precedence over a .aike one"
        );
    }

    #[test]
    fn resolves_zapi_extension_by_bare_name_and_index() {
        let d = TempDir::new("zapi2");
        d.write("mylib/index.aike");
        assert_eq!(
            resolve_in(d.path(), "mylib"),
            Some(d.0.join("mylib/index.aike")),
            "a bare specifier reaches a native extension"
        );
        d.write("single.aike");
        assert_eq!(
            resolve_in(d.path(), "single"),
            Some(d.0.join("single.aike"))
        );
    }

    /// Only a bare specifier can name a native module; `load_native` skips the
    /// registry query outright for everything else.
    #[test]
    fn only_bare_specifiers_can_name_a_native_module() {
        assert!(is_bare("fs"));
        assert!(is_bare("json-schema"));
        assert!(!is_bare(""));
        assert!(!is_bare("./a.cjs"));
        assert!(!is_bare("../a.cjs"));
        assert!(!is_bare("."));
        assert!(!is_bare(".."));
        assert!(!is_bare("/abs/a.cjs"));
    }
}
