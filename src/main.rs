// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#![allow(non_snake_case)]

mod ai;
mod aike;
mod browser;
mod class_event_emitter;
mod color;
#[cfg(feature = "enable_eval_log")]
mod eval_logger;
mod fs;
mod global;
mod gzip;
mod helpers;
mod html;
mod http;
mod inspect_value;
mod jinja2;
mod json_schema;
mod jsonl;
mod mcp;
mod path;
mod process;
mod repl;
mod transpile;
mod whatwg;
#[cfg(feature = "llama_cpp")]
mod worker;
mod yaml;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
};

use clap::{CommandFactory, Parser};
use color::{Color::*, colorize_println};
use zuqe::{
    EvalFlags, EvalType, Exception, ExitStatus, JSValue, ModuleDef, NativeObject, Runtime,
    ScopedContext,
};

#[cfg(feature = "llama_cpp")]
use crate::ai::{JSEngine, JSLlmContext, JSLlmModel, JSSamplerChain};

use crate::{
    ai::{JSLlmClient, init_mod_ai},
    aike::{AikeRuntimeState, JSCronJob, JSFileHandle, JSStdioHandle},
    browser::{JSPage, init_mod_browser},
    class_event_emitter::JSEventEmitter,
    fs::init_mod_fs,
    global::setup_globals,
    helpers::canonicalize_path,
    html::{HtmlDocument, HtmlElement, init_mod_html},
    http::init_mod_http,
    mcp::init_mod_mcp,
    path::init_mod_path,
    yaml::init_mod_yaml,
};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(version, disable_help_flag = true)]
struct Args {
    #[arg(short = 'h', long = "help", help = "Print help")]
    help: bool,

    #[arg(
        long,
        value_name = "n",
        help = "set memory limit to n (default: unlimited)"
    )]
    memory_limit: Option<String>,

    #[arg(
        long,
        value_name = "n",
        help = "set virtual stack arena size to n (default: 256k)"
    )]
    stack_arena: Option<String>,

    #[arg(
        long,
        value_name = "n",
        help = "set virtual stack byte limit to n (default: 512k)"
    )]
    stack_limit: Option<String>,

    #[arg(
        long,
        value_name = "n",
        help = "set C-stack recursion budget (max native stack below the eval SP); \
                omit/0 for auto mode derived from the thread stack bottom"
    )]
    stack_overflow: Option<String>,

    #[arg(short, long, default_value_t = false, help = "eval as esm module")]
    module: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "exit when the event loop drains; don't await the script's \
                completion-value promise (node-style; module TLA still waits)"
    )]
    no_await_result: bool,

    #[arg(
        short = 'e',
        long,
        value_name = "code",
        help = "eval script directly (no file)",
        conflicts_with = "filename"
    )]
    eval: Option<String>,

    #[arg(
        long,
        value_name = "path",
        help = "custom path for localStorage database file"
    )]
    local_storage_db: Option<String>,

    #[arg(help = "file to execute (omit for REPL mode)")]
    filename: Option<String>,

    #[arg(
        allow_hyphen_values = true,
        trailing_var_arg = true,
        help = "arguments to pass to script"
    )]
    script_args: Vec<String>,
}

fn parse_byte_size(ss: &str) -> Option<usize> {
    let i = ss
        .as_bytes()
        .iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(ss.len());

    if i > 0
        && let Ok(num) = ss[0..i].parse::<usize>()
    {
        if i == ss.len() - 1 {
            match ss.as_bytes()[i] {
                b'k' | b'K' => Some(num << 10),
                b'm' | b'M' => Some(num << 20),
                b'g' | b'G' => Some(num << 30),
                _ => None,
            }
        } else if i == ss.len() {
            Some(num)
        } else {
            None
        }
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(e) = dotenvy::dotenv() {
        #[cfg(debug_assertions)]
        eprintln!("[INFO] .env not loaded: {e}");
    }

    #[cfg(windows)]
    check_terminal();

    #[cfg(debug_assertions)]
    {
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open("debug.log")
            .unwrap();

        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Debug)
            .target(env_logger::Target::Pipe(Box::new(log_file)))
            .init();
    }

    let mut args = Args::parse();

    // --help without filename/eval: show aike help and exit
    // --help with filename/eval: pass through to script
    if args.help && args.filename.is_none() && args.eval.is_none() {
        let _ = Args::command().print_help();
        return ExitCode::SUCCESS;
    }
    if args.help {
        args.script_args.insert(0, "--help".to_string());
    }

    let filename: Option<String> = args.filename;

    // Build process.argv and process.execArgv
    let exe_path = std::env::current_exe()
        .ok()
        .and_then(|p| canonicalize_path(p).ok())
        .map(|p| p.to_string_lossy().to_string())
        .expect("failed to resolve current executable path");

    let argv: Vec<String> = {
        let mut v = Vec::with_capacity(2 + args.script_args.len());
        v.push(exe_path.clone());
        match &filename {
            Some(f) => v.push(f.clone()),
            None if args.eval.is_some() => v.push("[eval]".to_string()),
            None => v.push("<repl>".to_string()),
        }
        v.extend(args.script_args.iter().cloned());
        v
    };

    let exec_argv: Vec<String> = {
        let mut v = Vec::new();
        if args.module {
            v.push("--module".to_string());
        }
        if let Some(ref ml) = args.memory_limit {
            v.push(format!("--memory-limit={}", ml));
        }
        if let Some(ref sa) = args.stack_arena {
            v.push(format!("--stack-arena={}", sa));
        }
        if let Some(ref sl) = args.stack_limit {
            v.push(format!("--stack-limit={}", sl));
        }
        if let Some(ref so) = args.stack_overflow {
            v.push(format!("--stack-overflow={}", so));
        }
        v
    };

    // Create runtime, attaching aike's per-runtime host state (signal handlers,
    // and later other state formerly kept in thread-locals).
    let mut rt = Runtime::create_with(AikeRuntimeState::new()).unwrap();

    // Inject the tokio deadline waker: when the event loop parks with a
    // deadline (timer / quantum fallback), a sleep task wakes it. One armed
    // timer at a time — re-arming aborts the previous one.
    {
        let wake = rt.wake_handle();
        let armed: Arc<Mutex<Option<tokio::task::AbortHandle>>> = Arc::new(Mutex::new(None));
        rt.set_deadline_waker(std::sync::Arc::new(move |at: std::time::Instant| {
            let mut g = armed.lock().unwrap();
            if let Some(h) = g.take() {
                h.abort();
            }
            let w = wake.clone();
            *g = Some(
                tokio::spawn(async move {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                    w.wake();
                })
                .abort_handle(),
            );
        }));
    }

    // In release build, when set panic="abort" in Cargo.toml,
    // should install panic handler to ensure gc heap deallocate when panic.
    rt.install_panic_handler();

    // Kill `abortOnParentExit` children on process exit. atexit covers normal
    // exit and `process.exit`; the default signal handlers cover catchable
    // signals (which would otherwise terminate without running atexit). On
    // Windows, kill-on-close job objects + the console-ctrl handler cover the
    // same ground.
    install_exit_cleanup();

    // memory-limit (default: unlimited)
    if let Some(ref ss) = args.memory_limit {
        if let Some(size) = parse_byte_size(ss) {
            rt.gc_set_memory_limit(size);
        } else {
            colorize_println(format!("invalid memory-limit: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    // stack-arena (default: 256k)
    if let Some(ref ss) = args.stack_arena {
        if let Some(size) = parse_byte_size(ss) {
            if rt.set_stack_arena(size).is_err() {
                colorize_println(format!("cannot allocate stack-arena of size {ss}"), RedFg);
                std::process::exit(-1);
            }
        } else {
            colorize_println(format!("invalid stack-arena: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    // stack-limit (default: 512k)
    if let Some(ref ss) = args.stack_limit {
        if let Some(size) = parse_byte_size(ss) {
            rt.set_stack_limit(size);
        } else {
            colorize_println(format!("invalid stack-limit: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    // stack-overflow: omit/0 = auto mode (from thread stack bottom)
    if let Some(ref ss) = args.stack_overflow {
        if let Some(size) = parse_byte_size(ss) {
            rt.set_stack_budget(size);
        } else {
            colorize_println(format!("invalid stack-overflow: {ss}"), RedFg);
            std::process::exit(-1);
        }
    } else if !zuqe::Runtime::CAN_AUTO_STACK_BUDGET {
        // Platform can't report the stack bottom: auto mode would leave no
        // C-stack guard, so fall back to a strict 512 KiB budget.
        rt.set_stack_budget(512 << 10);
    }

    rt.set_module_loader(
        Some(js_module_normalizer),
        js_module_loader,
        std::ptr::null_mut(),
    );

    crate::aike::register_runtime_traces(&mut rt);

    // create context and setup
    let mut ctx = rt
        .create_context(move |cx| {
            ScopedContext::ECMA_CONTEXT_SETUP(cx)?;

            // use classes
            cx.use_classes(&[
                JSEventEmitter::class_id(),
                JSStdioHandle::class_id(),
                JSFileHandle::class_id(),
                JSCronJob::class_id(),
                HtmlDocument::class_id(),
                HtmlElement::class_id(),
                JSPage::class_id(),
                crate::http::client::headers::HeaderListIterator::class_id(),
                crate::http::client::cache_storage::JSCacheStorage::class_id(),
                crate::http::client::cache_storage::JSCache::class_id(),
                JSLlmClient::class_id(),
            ])?;

            #[cfg(feature = "llama_cpp")]
            cx.use_classes(&[
                JSLlmModel::class_id(),
                JSLlmContext::class_id(),
                JSEngine::class_id(),
                JSSamplerChain::class_id(),
            ])?;

            // initialize native modules
            init_mod_fs(cx)?;
            init_mod_http(cx)?;
            init_mod_path(cx)?;
            init_mod_html(cx)?;
            init_mod_browser(cx)?;
            init_mod_yaml(cx)?;
            jsonl::init_mod_jsonl(cx)?;
            json_schema::init_mod_json_schema(cx)?;
            jinja2::init_mod_jinja2(cx)?;
            init_mod_ai(cx)?;
            init_mod_mcp(cx)?;

            setup_globals(
                cx,
                &argv,
                &exec_argv,
                &exe_path,
                #[cfg(feature = "local_storage")]
                args.local_storage_db.as_deref(),
            )
        })
        .inspect_err(|_| {
            let _ = rt.create_context(|cx| {
                let exc = cx.take_exception();
                let t = exc.to_text(cx);
                println!("{:?}", t);
                Ok(())
            });
        })
        .unwrap();

    let script: String;
    let eval_name: zuqe::EvalName;
    let is_module: bool;

    if let Some(ref_filename) = filename {
        //
        // Run script file
        //
        let (script_bytes, script_path) = match read_script(&ref_filename) {
            Ok((buf, name)) => (buf, name),
            Err(e) => {
                colorize_println(e, RedFg);
                std::process::exit(-1);
            }
        };
        let script_str = String::from_utf8_lossy(&script_bytes).to_string();

        let (transpiled, detected_module): (String, bool) =
            if transpile::needs_transpile(&script_path) {
                match transpile::transpile(&script_str) {
                    Ok(result) => (result.code, result.is_module),
                    Err(e) => {
                        colorize_println(
                            format!("error: failed to transpile '{script_path}': {e}"),
                            RedFg,
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                // Non-TS files: use zuqe's built-in module detection
                let detected = ModuleDef::detect_module(script_str.as_bytes());
                (script_str, detected)
            };

        let is_module_value = detected_module
            || args.module
            || ref_filename.ends_with(".mjs")
            || ref_filename.ends_with(".mts");

        // Convert the entry filename to a canonical `file://` URL for module resolution.
        // Module names (and `import.meta.url`) are URLs, and a `file://` base is also
        // required so that dynamic `import()` resolves relative specifiers correctly.
        let url =
            zuqe::path_to_url(&ref_filename).expect("entry path must convert to a file:// URL");

        script = transpiled;
        eval_name = zuqe::EvalName::BaseUrl(url);
        is_module = is_module_value;
    } else if let Some(eval_code) = args.eval {
        //
        // Eval mode: inline script
        //
        script = eval_code;
        eval_name = zuqe::EvalName::Name("eval".to_string());
        is_module = args.module;
    } else {
        //
        // REPL mode: run repl task and exit.
        //
        return repl::run(rt, ctx).await;
    };

    // Register the root runtime/context so child runtimes can attach to it and
    // child → parent messages can queue delivery jobs on it. The engine drives
    // the whole runtime tree from the root's tick, so a single `eval_wait`
    // suffices.
    crate::aike::set_root_runtime(rt.clone());
    crate::aike::set_root_ctx(ctx);

    let result = ctx
        .eval_wait(
            &mut rt,
            script.as_bytes(),
            &eval_name,
            if is_module {
                EvalType::Module
            } else {
                EvalType::Global
            },
            if args.no_await_result {
                EvalFlags::NO_AWAIT_RESULT
            } else {
                EvalFlags::empty()
            },
        )
        .await;

    // Resolve the process exit code. We exit explicitly (not by returning an
    // `ExitCode` from main) because `#[tokio::main]` would otherwise block in
    // tokio's graceful runtime shutdown waiting for spawned background tasks
    // that never end (e.g. a `process.stdin.onData` read loop whose pipe never
    // hits EOF). That hang is exactly why `process.exit` did not terminate a
    // process that had registered stdin readers. Each arm yields the proper i32
    // code; the interrupt arm reads the code captured by `process.exit`
    // (ExitCode itself is opaque and not extractable).
    let exit_code: i32 = match result {
        ExitStatus::Value(_) => 0,
        ExitStatus::Exception(Exception::Error(err)) => {
            ctx.root_scope().with_new_scope(|cx| {
                let text = Exception::Error(err).to_text(cx);
                colorize_println(text.message, RedFg);
                if let Some(tb) = text.traceback {
                    eprintln!("{}", tb);
                }
            });
            1
        }
        ExitStatus::Interrupted(_) => {
            #[cfg(debug_assertions)]
            log::debug!("Process interrupted");
            crate::process::PROCESS_EXIT_CODE.load(std::sync::atomic::Ordering::SeqCst)
        }
        ExitStatus::Exception(Exception::OutOfMemory) => {
            colorize_println("out of memory", RedFg);
            1
        }
        ExitStatus::Exception(
            Exception::No | Exception::TooManyClasses | Exception::TooManyAtoms,
        ) => {
            unreachable!();
        }
    };

    // Drop the engine runtime BEFORE exiting so GC nodes are finalized, then
    // exit without waiting on orphaned tokio tasks. (`ctx` is a Copy handle, so
    // only `rt` holds the heap that needs finalizing.)
    drop(rt);

    #[cfg(feature = "llama_cpp")]
    ai::free_backend();

    // Kill `abortOnParentExit` children synchronously: every script exit
    // (normal end, process.exit, engine error) funnels through here.
    // std::process::exit maps to ExitProcess on Windows, which skips CRT
    // atexit entirely, so this must happen explicitly before it.
    crate::aike::kill_all_children();
    crate::mcp::kill_all_mcp_children();
    crate::browser::kill_all_spawned_browsers();

    std::process::exit(exit_code);
}

/// Install exit cleanup for `abortOnParentExit` children.
///
/// Unix: `atexit` runs on normal exit and `std::process::exit` (libc::exit),
/// covering paths that bypass main's epilogue; default signal handlers cover
/// SIGTERM/SIGINT/SIGHUP/SIGQUIT. Windows: main's epilogue kills explicitly
/// (ExitProcess skips atexit), plus the console-ctrl handler covers Ctrl-C,
/// console close, logoff and shutdown; kill-on-close job objects remain as
/// crash-path backstop.
fn install_exit_cleanup() {
    #[cfg(not(windows))]
    {
        extern "C" fn exit_kill_children() {
            crate::aike::kill_all_children();
            crate::mcp::kill_all_mcp_children();
            crate::browser::kill_all_spawned_browsers();
        }
        unsafe {
            libc::atexit(exit_kill_children);
        }
    }
    #[cfg(windows)]
    {
        // Console ctrl events terminate the process without running main's
        // epilogue, so children must be killed here — same cleanup set.
        unsafe extern "system" fn console_ctrl_handler(ctrl: u32) -> i32 {
            crate::aike::kill_all_children();
            crate::mcp::kill_all_mcp_children();
            crate::browser::kill_all_spawned_browsers();

            // 128+ctrl mirrors the Unix 128+signal convention. Never returns.
            unsafe { windows_sys::Win32::System::Threading::ExitProcess(128 + ctrl) }
        }

        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(console_ctrl_handler),
                1,
            );
        }
    }

    crate::aike::install_default_signal_handlers();
}

/// Extract the `type` value from import attributes, returning `None` if
/// the attrs object is absent or has no `type` property.
fn get_attr_type(ctx: &mut ScopedContext, attrs: JSValue) -> Option<String> {
    if !attrs.is_object() {
        return None;
    }
    attrs.get_property_str(ctx, "type").ok().and_then(|v| {
        v.as_string(true)
            .map(|s| s.content().encode_utf8(false).to_string())
    })
}

/// Module name normalizer: resolves bare specifiers (e.g. `mymod`,
/// `vendor/lib`) via the (importer dir, CWD, AIKE_LIB_PATH) search; everything
/// else keeps the engine's default base-relative resolution.
///
/// The search runs here (not in the loader) so the module is compiled under
/// the real file URL: that keeps cache identity stable across repeated
/// imports, makes `import.meta.url` point at the actual file, lets relative
/// imports inside the module resolve correctly, and lets the loader dispatch
/// searched `.aike` hits to `load_zapi_extension`.
fn js_module_normalizer(
    _ctx: &mut ScopedContext,
    base_name: &url::Url,
    name: &str,
    _: *mut std::ffi::c_void,
) -> Result<url::Url, i32> {
    if is_bare_specifier(name) {
        // Importer dir first so module-local deps are not hijacked by CWD.
        let mut dirs = Vec::new();
        if let Some(p) = zuqe::url_to_abspath(base_name)
            && let Some(parent) = p.parent()
        {
            dirs.push(parent.to_path_buf());
        }
        dirs.extend(module_search_dirs());

        if let Some(path) = search_module(name, &dirs) {
            return zuqe::path_to_url(&path.to_string_lossy()).map_err(|_| -1);
        }
    }

    // Default resolution (mirrors js_default_module_normalize_name).
    if Path::new(name).is_absolute() {
        zuqe::path_to_url(name).map_err(|_| -1)
    } else {
        base_name.join(name).map_err(|_| -1)
    }
}

/// A bare specifier: not empty, not relative (`./x`, `../x`), not absolute,
/// not a URL — only these go through the search-path resolution.
fn is_bare_specifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !Path::new(name).is_absolute()
        && url::Url::parse(name).is_err()
}

/// Directories searched for bare specifiers, in order: CWD, then
/// `$AIKE_LIB_PATH` (single directory, skipped when unset or empty).
fn module_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    if let Ok(lib) = std::env::var("AIKE_LIB_PATH")
        && !lib.is_empty()
    {
        dirs.push(PathBuf::from(lib));
    }
    dirs
}

/// Resolve a bare specifier against `dirs`, returning the first existing file.
///
/// Per directory (in order): if the joined path is a directory (dotted names
/// like `abc.def` can be directories too), look for `index.js` / `index.ts`
/// inside it; otherwise try `name.js`, `name.ts`, `name.aike` (native
/// zapi extension), in that order.
///
/// The first hit wins; a directory without index files falls through to the
/// next directory. `None` means not found in any directory.
fn search_module(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    // Extension is taken from the last path component so dotted directories
    // (e.g. `my.pkg/mod`) don't make the specifier look extension-terminated.
    let has_ext = Path::new(name)
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f.rsplit_once('.').is_some_and(|(_, e)| !e.is_empty()));

    for dir in dirs {
        let cand = dir.join(name);
        // dir check first: `abc.def/` is a valid directory name
        if cand.is_dir() {
            for f in ["index.js", "index.ts"] {
                let p = cand.join(f);
                if p.is_file() {
                    return Some(p);
                }
            }
            continue;
        }

        if has_ext {
            if cand.is_file() {
                return Some(cand);
            }
        } else {
            for ext in ["js", "ts", "aike"] {
                let p = cand.with_extension(ext);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    None
}

fn js_module_loader(
    ctx: &mut ScopedContext,
    file_name: &url::Url,
    attrs: JSValue,
    _: *mut std::ffi::c_void,
) -> Result<ModuleDef, i32> {
    // Convert URL back to absolute path for filesystem access.
    let Some(file_path) = zuqe::url_to_abspath(file_name) else {
        return Err(ctx.throw_reference_error(&format!(
            "could not load module '{file_name}': not a file:// URL"
        )));
    };

    let is_native_lib = file_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "aike");

    if is_native_lib {
        return ctx.load_zapi_extension(&file_path);
    }

    let attr_type = get_attr_type(ctx, attrs);

    // Use resolved filename (may be .ts fallback from .js)
    let file_path_str = file_path.as_os_str().to_str().unwrap();
    let resolved = read_script(file_path_str);
    let (buf, resolved_name) = match resolved {
        Ok((b, n)) => (b, n),
        Err(e) => {
            return Err(
                ctx.throw_reference_error(&format!("could not load module '{file_name}'. {e}"))
            );
        }
    };

    // `resolved_name` is the path actually read from disk; it differs from
    // `file_path_str` only when a `.js` import falls back to `.ts` (or
    // `.mjs`→`.mts` / `.cjs`→`.cts`). That fallback is expected and legal —
    // the canonical module name stays `file_name` (used below in
    // `compile_module`), so `resolved_name` must never be used as the key.
    // Assert agreement only when no fallback happened, which still guards
    // against accidentally using `resolved_name` as the canonical name.
    #[cfg(debug_assertions)]
    if ts_fallback(file_path_str).as_deref() != Some(resolved_name.as_str()) {
        debug_assert_eq!(resolved_name, file_path_str);
    }

    // Import-attribute `type` dispatch (mirrors zjs): `json` / `text` wrap the
    // content as a default export; any other type errors; `None` falls through
    // to normal compilation.
    match attr_type.as_deref() {
        Some("json") => {
            // JSON module: wrap as `export default <json>;\n`
            let content = String::from_utf8_lossy(&buf);
            let mut src = String::from("export default ");
            src.push_str(&content);
            src.push('\n');
            ctx.compile_module(src.as_bytes(), file_name, false)
        }
        Some("text") => {
            // Text module: wrap as `export default "<escaped>";\n`
            let content = String::from_utf8_lossy(&buf);
            let mut src = String::from("export default \"");
            src.push_str(&escape_js_string(&content));
            src.push_str("\";\n");
            ctx.compile_module(src.as_bytes(), file_name, false)
        }
        Some(a) => Err(ctx.throw_type_error(&format!("unsupported import attribute type '{a}'"))),
        None => {
            let code = if transpile::needs_transpile(&resolved_name) {
                if resolved_name.ends_with(".d.ts") {
                    return ctx.compile_module(b"export {}", file_name, false);
                }

                let source = String::from_utf8_lossy(&buf);
                match transpile::transpile(&source) {
                    Ok(result) => result.code.into_bytes(),
                    Err(e) => {
                        return Err(ctx.throw_syntax_error(&format!(
                            "failed to transpile module '{resolved_name}': {e}"
                        )));
                    }
                }
            } else {
                buf.to_vec()
            };

            // Note: should use original file URL (not resolved path) as module name
            ctx.compile_module(&code, file_name, false)
        }
    }
}

/// Escape a string for embedding inside a JS double-quoted string literal.
fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x00'..='\x1f' => out.push_str(&format!("\\x{:02x}", ch as u8)),
            c => out.push(c),
        }
    }
    out
}

/// Read file with JS → TS fallback.
/// If `foo.js` not found, tries `foo.ts`. Same for `.mjs` → `.mts`, `.cjs` → `.cts`.
/// Returns (content bytes, resolved filename).
fn read_script(file_name: &str) -> Result<(Vec<u8>, String), String> {
    // Try exact path first
    if let Ok(buf) = std::fs::read(file_name) {
        return Ok((buf, file_name.to_string()));
    }

    // Try TS/MTS/CTS fallback
    if let Some(ts_name) = ts_fallback(file_name)
        && let Ok(buf) = std::fs::read(&ts_name)
    {
        return Ok((buf, ts_name));
    }

    // Report original error
    match std::fs::read(file_name) {
        Ok(buf) => Ok((buf, file_name.to_string())),
        Err(e) => Err(format!("cannot load '{file_name}': {e}")),
    }
}

/// Map `.js` → `.ts`, `.mjs` → `.mts`, `.cjs` → `.cts`
fn ts_fallback(file_name: &str) -> Option<String> {
    if let Some(base) = file_name.strip_suffix(".js") {
        Some(format!("{}.ts", base))
    } else if let Some(base) = file_name.strip_suffix(".mjs") {
        Some(format!("{}.mts", base))
    } else {
        file_name
            .strip_suffix(".cjs")
            .map(|base| format!("{}.cts", base))
    }
}

/// Check if we're running in a legacy Windows console (conhost.exe)
/// and suggest a modern terminal if so.
#[cfg(windows)]
fn check_terminal() {
    // Modern terminals set these env vars:
    //   TERM_PROGRAM=vscode       — VS Code
    //   TERM_PROGRAM=wt           — Windows Terminal
    //   WT_SESSION=...            — Windows Terminal
    //   ConEmuANSI=ON             — ConEmu
    //   ALACRITTY_LOG=...         — Alacritty
    //   WEZTERM_EXECUTABLE=...    — WezTerm
    //   TABBY_CONFIG_DIR=...      — Tabby
    //
    // Classic conhost.exe (cmd / powershell.exe) sets none of these.
    let is_modern = std::env::var("TERM_PROGRAM").is_ok()
        || std::env::var("WT_SESSION").is_ok()
        || std::env::var("ConEmuANSI").is_ok()
        || std::env::var("ALACRITTY_LOG").is_ok()
        || std::env::var("WEZTERM_EXECUTABLE").is_ok()
        || std::env::var("TABBY_CONFIG_DIR").is_ok();

    if !is_modern {
        eprintln!(
            "[TIP] You appear to be running in the classic Windows console (conhost.exe).\
            \n      Some content (e.g. emoji) may not render correctly.\
            \n      Consider using VS Code terminal, Windows Terminal, or WezTerm for the best experience.\
            \n"
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    /// Self-cleaning scratch dir; `write(rel)` creates parent dirs.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "aike_search_{}_{}_{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn write(&self, rel: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "").unwrap();
        }
        fn mkdir(&self, rel: &str) {
            std::fs::create_dir_all(self.0.join(rel)).unwrap();
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

    fn find(name: &str, dirs: &[&TempDir]) -> Option<PathBuf> {
        let owned: Vec<PathBuf> = dirs.iter().map(|d| d.0.clone()).collect();
        search_module(name, &owned)
    }

    #[test]
    fn finds_js_extension() {
        let d = TempDir::new("js");
        d.write("foo.js");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.js")));
    }

    #[test]
    fn js_beats_ts_beats_aike() {
        let d = TempDir::new("order1");
        d.write("foo.js");
        d.write("foo.ts");
        d.write("foo.aike");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.js")));

        let d = TempDir::new("order2");
        d.write("foo.ts");
        d.write("foo.aike");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.ts")));

        let d = TempDir::new("order3");
        d.write("foo.aike");
        assert_eq!(find("foo", &[&d]), Some(d.0.join("foo.aike")));

        // raw platform suffixes are not module candidates
        let d = TempDir::new("order4");
        d.write("foo.dylib");
        d.write("foo.so");
        d.write("foo.dll");
        assert_eq!(find("foo", &[&d]), None);
    }

    #[test]
    fn dir_uses_index_files() {
        let d = TempDir::new("index1");
        d.write("pkg/index.js");
        d.write("pkg/index.ts");
        d.write("pkg.js");
        // directory wins over sibling extension candidates
        assert_eq!(find("pkg", &[&d]), Some(d.0.join("pkg/index.js")));

        let d = TempDir::new("index2");
        d.write("pkg/index.ts");
        assert_eq!(find("pkg", &[&d]), Some(d.0.join("pkg/index.ts")));
    }

    #[test]
    fn dir_without_index_falls_through_to_next_dir() {
        let d1 = TempDir::new("fall1");
        d1.mkdir("pkg");
        let d2 = TempDir::new("fall2");
        d2.write("pkg.js");
        assert_eq!(find("pkg", &[&d1, &d2]), Some(d2.0.join("pkg.js")));
    }

    #[test]
    fn extension_name_is_literal_only() {
        let d = TempDir::new("lit1");
        d.write("bar.js");
        assert_eq!(find("bar.js", &[&d]), Some(d.0.join("bar.js")));

        // has extension → no .ts fallback in search (loader fallback covers that)
        let d = TempDir::new("lit2");
        d.write("bar.ts");
        assert_eq!(find("bar.js", &[&d]), None);
    }

    #[test]
    fn first_dir_wins() {
        let d1 = TempDir::new("first1");
        let d2 = TempDir::new("first2");
        d2.write("foo.js");
        assert_eq!(find("foo", &[&d1, &d2]), Some(d2.0.join("foo.js")));
        d1.write("foo.ts");
        assert_eq!(find("foo", &[&d1, &d2]), Some(d1.0.join("foo.ts")));
    }

    #[test]
    fn subpath_specifier() {
        let d = TempDir::new("sub1");
        d.write("vendor/lib.js");
        assert_eq!(find("vendor/lib", &[&d]), Some(d.0.join("vendor/lib.js")));

        let d = TempDir::new("sub2");
        d.write("vendor/lib/index.js");
        assert_eq!(
            find("vendor/lib", &[&d]),
            Some(d.0.join("vendor/lib/index.js"))
        );
    }

    #[test]
    fn dotted_dir_name_gets_index_lookup() {
        let d = TempDir::new("dotdir");
        d.write("abc.def.js");
        d.write("abc.def/index.js");
        // directory check wins over the sibling extension candidate
        assert_eq!(find("abc.def", &[&d]), Some(d.0.join("abc.def/index.js")));
    }

    #[test]
    fn dotted_dir_component_is_not_extension() {
        let d = TempDir::new("dot1");
        d.write("my.pkg/mod.js");
        assert_eq!(find("my.pkg/mod", &[&d]), Some(d.0.join("my.pkg/mod.js")));

        // "my.pkg" itself ends with an extension → literal lookup
        let d = TempDir::new("dot2");
        d.write("my.pkg");
        assert_eq!(find("my.pkg", &[&d]), Some(d.0.join("my.pkg")));
    }

    #[test]
    fn not_found() {
        let d = TempDir::new("none1");
        d.mkdir("pkg");
        assert_eq!(find("missing", &[&d]), None);
        assert_eq!(find("pkg", &[&d]), None); // dir without index files
    }

    #[test]
    fn bare_specifier_detection() {
        assert!(is_bare_specifier("mymod"));
        assert!(is_bare_specifier("vendor/lib"));
        assert!(is_bare_specifier("mylib.dylib"));
        assert!(!is_bare_specifier("./mymod"));
        assert!(!is_bare_specifier("../mymod"));
        assert!(!is_bare_specifier("/abs/mymod.js"));
        assert!(!is_bare_specifier("file:///abs/mymod.js"));
        assert!(!is_bare_specifier(""));
    }
}
