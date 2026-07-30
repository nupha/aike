// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#![allow(non_snake_case)]

mod aike;
mod browser;
mod class_event_emitter;
mod color;
mod fs;
mod global;
mod helpers;
mod html;
mod http;
mod inspect_value;
mod jsonl;
mod llm;
mod mcp;
mod path;
mod repl;
mod transpile;
mod whatwg;
#[cfg(feature = "llama_cpp")]
mod worker;
mod yaml;

use std::{io::Write, process::ExitCode};

use clap::{CommandFactory, Parser};
use color::{Color::*, colorize_println};
use linkme::distributed_slice;
use zuqe::{
    ClassID, EvalFlags, EvalType, Exception, ExitStatus, JSValue, ModuleDef, NativeObject, Runtime,
    ScopedContext, builtins::promise::PromiseFuture,
};

use crate::{llm::init_mod_llm, mcp::gc_trace_stdio_state};

#[cfg(feature = "llama_cpp")]
use crate::llm::{JSEngine, JSLlmContext, JSLlmModel, JSSamplerChain};

use crate::{
    aike::{JSCronJob, JSFileHandle, JSStdioHandle, gc_trace_cron, gc_trace_io},
    browser::{JSBrowser, JSPage, gc_trace_browser, init_mod_browser},
    class_event_emitter::JSEventEmitter,
    fs::init_mod_fs,
    global::setup_globals,
    helpers::canonicalize_path,
    html::{HtmlDocument, HtmlElement, init_mod_html},
    http::init_mod_http,
    llm::JSLlmClient,
    mcp::init_mod_mcp,
    path::init_mod_path,
    whatwg::stream::gc_trace_stream,
    whatwg::timeout::gc_trace_timeout,
    yaml::init_mod_yaml,
};

/// write data both to stderr and file
struct TeeWriter<A: Write, B: Write> {
    a: Option<A>,
    b: Option<B>,
}

impl<A: Write, B: Write> TeeWriter<A, B> {
    fn new(a: Option<A>, b: Option<B>) -> Self {
        Self { a, b }
    }
}

impl<A: Write, B: Write> Write for TeeWriter<A, B> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(w) = &mut self.a {
            Some(w.write_all(buf)?)
        } else {
            None
        };
        if let Some(w) = &mut self.b {
            Some(w.write_all(buf)?)
        } else {
            None
        };

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(w) = &mut self.a {
            w.flush()?;
        }
        if let Some(w) = &mut self.b {
            w.flush()?;
        }
        Ok(())
    }
}

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
        help = "set C-stack recursion limit to n (default: 512k)"
    )]
    stack_overflow: Option<String>,

    #[arg(short, long, default_value_t = false, help = "eval as esm module")]
    module: bool,

    #[arg(
        short = 'e',
        long,
        value_name = "code",
        help = "eval script directly (no file)",
        conflicts_with = "filename"
    )]
    eval: Option<String>,

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

#[distributed_slice]
pub(crate) static USER_CLASS_DEFS: [fn(&mut Runtime) -> Result<ClassID, i32>];

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

        let tee = TeeWriter::new(Some(log_file), Option::<std::fs::File>::None);

        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Debug)
            .target(env_logger::Target::Pipe(Box::new(tee)))
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

    // Create runtime
    let mut rt = Runtime::create().unwrap();

    // In release build, when set panic="abort" in Cargo.toml,
    // should install panic handler to ensure gc heap deallocate when panic.
    rt.install_panic_handler();

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

    // stack-overflow (default: 256k)
    if let Some(ref ss) = args.stack_overflow {
        if let Some(size) = parse_byte_size(ss) {
            rt.set_stack_overflow(size);
        } else {
            colorize_println(format!("invalid stack-overflow: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    rt.set_module_loader(None, js_module_loader, std::ptr::null_mut());

    rt.gc_set_trace_handler(|gcx| {
        aike::signal::gc_trace_zuq_signals(gcx);
        gc_trace_timeout(gcx);
        gc_trace_cron(gcx);
        gc_trace_io(gcx);
        gc_trace_stdio_state(gcx);
        gc_trace_browser(gcx);
        gc_trace_stream(gcx);
    });

    // register native classes
    for class_reg_fn in USER_CLASS_DEFS {
        class_reg_fn(&mut rt).unwrap();
    }

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
                JSBrowser::class_id(),
                JSPage::class_id(),
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
            init_mod_llm(cx)?;
            init_mod_mcp(cx)?;

            setup_globals(cx, &argv, &exec_argv, &exe_path)
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
    let entry_url: String;
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
        let url = zuqe::path_to_url(&ref_filename)
            .expect("entry path must convert to a file:// URL")
            .to_string();

        script = transpiled;
        entry_url = url;
        is_module = is_module_value;
    } else if let Some(eval_code) = args.eval {
        //
        // Eval mode: inline script
        //
        script = eval_code;
        entry_url = "eval".to_string();
        is_module = args.module;
    } else {
        //
        // REPL mode: run repl task and exit.
        //
        return repl::run(rt, ctx).await;
    };

    let mut promise_future: Option<PromiseFuture> = None;

    let r = ctx
        .eval(
            script.as_bytes(),
            &entry_url,
            if is_module {
                EvalType::Module
            } else {
                EvalType::Global
            },
            EvalFlags::empty(),
        )
        .map(|v| v.as_object_Promise());

    let res = match r {
        Ok(Some(p)) => match p.into_future(&mut ctx.root_scope()) {
            Ok(fut) => {
                promise_future = Some(fut);
                Ok(())
            }
            Err(_) => Err(-1),
        },
        Ok(None) => Ok(()),
        Err(_) => Err(-1),
    };

    if res.is_err() {
        let code = match ctx.take_exception() {
            Exception::Error(e) => {
                let text = Exception::Error(e).to_text(&mut ctx.root_scope());
                colorize_println(text.message, GreenFg);
                if let Some(tb) = text.traceback {
                    eprintln!("{}", tb);
                }
                // value 'e' is in cx0 (top-level scope), it won't be gc collected.
                // we should remove it from protection manually.
                if let Some(n) = e.gc_node() {
                    ctx.root_scope().remove_local(n.into());
                }
                1
            }
            Exception::OutOfMemory => {
                colorize_println("out of memory", RedFg);
                1
            }
            Exception::TooManyAtoms => {
                colorize_println("too many atoms", RedFg);
                1
            }
            Exception::TooManyClasses => {
                colorize_println("too many classes", RedFg);
                1
            }
            Exception::No => {
                #[cfg(debug_assertions)]
                unreachable!();
                #[cfg(not(debug_assertions))]
                0
            }
        };

        return ExitCode::from(code);
    }

    // await till all background jobs are finished
    //
    // Drive the runtime event loop (`run_async`) together with the script's
    // top-level promise. Exit as soon as the runtime reports back: `run_async`
    // returns `Interrupted` once the interrupt flag is set (e.g. via
    // `Aike.exit`). We must NOT wait for `fut` in that case — `fut` only
    // resolves after `run_async` ticks it, so if `run_async` has already
    // returned on interrupt, `fut` can never progress and the process would
    // hang forever.
    //
    // `rt_fut` is polled from a single site at any time (either the `select!`
    // here, or the `rt_fut.await` after the script promise settles) to avoid
    // double-polling the same future, which would let one poll consume the
    // interrupt while the other is left pending.
    let result = if let Some(fut) = promise_future {
        let mut rt_fut = std::pin::pin!(rt.run_async());
        let mut script = Some(fut);

        loop {
            if let Some(s) = script.take() {
                tokio::select! {
                    rt_res = &mut rt_fut => {
                        break rt_res;
                    },

                    promise_res = s => {
                        match promise_res {
                            Ok(_) => {
                                // suppress eval result printing
                            }
                            Err(reason) => {
                                // Promise reject
                                ctx.root_scope().with_new_scope(|cx| {
                                    let text = Exception::Error(reason).to_text(cx);
                                    colorize_println(text.message, RedFg);
                                    if let Some(tb) = text.traceback {
                                        eprintln!("{}", tb);
                                    }
                                });
                            }
                        }
                        // Script settled; drop it and keep driving the runtime
                        // until it returns (interrupt / drained).
                    }
                }
            } else {
                break rt_fut.await;
            }
        }
    } else {
        rt.run_async().await
    };

    match result {
        ExitStatus::Value(_) => ExitCode::SUCCESS,
        ExitStatus::Exception(Exception::Error(err)) => {
            ctx.root_scope().with_new_scope(|cx| {
                let text = Exception::Error(err).to_text(cx);
                colorize_println(text.message, RedFg);
                if let Some(tb) = text.traceback {
                    eprintln!("{}", tb);
                }
            });
            ExitCode::FAILURE
        }
        ExitStatus::Interrupted(code) => {
            #[cfg(debug_assertions)]
            log::debug!("Process interrupted with code {:?}", code);
            code
        }
        ExitStatus::Exception(Exception::OutOfMemory) => {
            colorize_println("out of memory", RedFg);
            ExitCode::FAILURE
        }
        ExitStatus::Exception(
            Exception::No | Exception::TooManyClasses | Exception::TooManyAtoms,
        ) => {
            unreachable!();
        }
    }
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

    let file_path_str = file_path.to_string_lossy().to_string();

    if file_path_str.ends_with(".dylib")
        || file_path_str.ends_with(".so")
        || file_path_str.ends_with(".dll")
    {
        Err(ctx.throw_internal_error("dynamic library loading not implemented"))
    } else {
        // Check for import attributes: type: "json"
        let is_json = attrs.is_object()
            && attrs
                .get_property_str(ctx, "type")
                .map(|v| {
                    v.as_string(true)
                        .is_some_and(|s| s.content().encode_utf8(false).as_ref() == "json")
                })
                .unwrap_or(false);

        // Use resolved filename (may be .ts fallback from .js)
        let resolved = read_script(&file_path_str);
        let (buf, resolved_name) = match resolved {
            Ok((b, n)) => (b, n),
            Err(e) => {
                return Err(
                    ctx.throw_reference_error(&format!("could not load module '{file_name}'. {e}"))
                );
            }
        };

        if is_json {
            // JSON module: wrap as `export default <json>;\n`
            let content = String::from_utf8_lossy(&buf);
            let mut src = String::from("export default ");
            src.push_str(&content);
            src.push('\n');
            ctx.compile_module(src.as_bytes(), file_name.as_str(), false)
        } else {
            let code = if transpile::needs_transpile(&resolved_name) {
                if resolved_name.ends_with(".d.ts") {
                    return ctx.compile_module(b"export {}", &resolved_name, false);
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
            ctx.compile_module(&code, file_name.as_str(), false)
        }
    }
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
    } else if let Some(base) = file_name.strip_suffix(".cjs") {
        Some(format!("{}.cts", base))
    } else {
        None
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
