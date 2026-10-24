// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

#![allow(non_snake_case)]

mod ai;
mod aike;
mod builtin_modules;
mod cjs;
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
mod module_loader;
mod os;
mod path;
mod process;
mod repl;
mod transpile;
mod whatwg;
mod yaml;

use std::{
    process::ExitCode,
    sync::{Arc, Mutex},
};

use clap::{CommandFactory, Parser};
use color::{Color::*, colorize_println};
use zuqe::{EvalFlags, EvalType, Exception, ExitStatus, NativeObject, Runtime, ScopedContext};

use crate::{
    aike::{AikeRuntimeState, JSCronJob, JSStdioHandle},
    class_event_emitter::JSEventEmitter,
    global::setup_globals,
    helpers::canonicalize_path,
};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(version = process::info::version_banner(), disable_help_flag = true)]
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
        help = "set VM stack arena size to n (default: 256k)"
    )]
    vm_stack_arena: Option<String>,

    #[arg(
        long,
        value_name = "n",
        help = "set VM stack byte limit to n (default: 512k)"
    )]
    vm_stack_limit: Option<String>,

    #[arg(
        long,
        value_name = "n",
        help = "set C-stack recursion limit (max native stack below the eval SP); \
                omit/0 for auto mode derived from the thread stack bottom"
    )]
    thread_stack_limit: Option<String>,

    #[arg(
        long,
        value_name = "n",
        help = "run the engine on a thread with an n-byte native stack \
                (default: the main thread's platform default); raise it when deep \
                native recursion trips 'stack overflow'; 0 = default"
    )]
    thread_stack: Option<String>,

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

/// Empirical floor for `--thread-stack <n>`, an aike-side policy (not an engine
/// contract): below this the engine has too little native stack to initialize
/// — the C-stack guard reserves `THREAD_STACK_LIMIT_MARGIN` head-room near the
/// stack bottom, and native builtins re-entering JS are expensive.
const MIN_THREAD_STACK: usize = 1 << 20;

fn main() -> ExitCode {
    #[cfg(target_os = "windows")]
    unsafe {
        // Makes the process per-monitor DPI aware, so Windows stops scaling the window
        // bitmap.
        //
        // A DPI-unaware process is served a 96 DPI lie: `GetDpiForWindow` answers 96,
        // native windows render at their logical size, and the compositor then resamples
        // that bitmap up to the monitor's scale. At 125% or 150% that resample is what
        // makes text look soft.
        //
        // The usual home for this is the executable's manifest; this binary carries
        // none, so it asks here instead. `main` runs before the first window exists,
        // which is the only time the call is allowed. A manifest, if one ever appears,
        // wins and makes the call fail — the state we want anyway.
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }

    let args = Args::parse();

    // `--thread-stack <n>`: run the whole engine on a thread whose native stack
    // is `n` bytes. The C-stack guard is sized from the *running* thread's
    // stack, so this is the only way to lift the ceiling that reports
    // `stack overflow` on deep native recursion. `0`/omitted keeps the main
    // thread's platform-default stack.
    let stack_size = match args.thread_stack.as_deref() {
        None => None,
        Some(ss) => match parse_byte_size(ss) {
            Some(0) => None,
            Some(n) if n < MIN_THREAD_STACK => {
                colorize_println(
                    format!(
                        "thread-stack must be at least {} bytes ({} is too small \
                         for the C-stack guard)",
                        MIN_THREAD_STACK, ss
                    ),
                    RedFg,
                );
                std::process::exit(-1);
            }
            Some(n) => Some(n),
            None => {
                colorize_println(format!("invalid thread-stack: {ss}"), RedFg);
                std::process::exit(-1);
            }
        },
    };

    let Some(size) = stack_size else {
        return block_on(run(args));
    };

    match std::thread::Builder::new()
        .name("aike".to_string())
        .stack_size(size)
        .spawn(move || block_on(run(args)))
    {
        Ok(handle) => handle.join().unwrap_or(ExitCode::FAILURE),
        Err(e) => {
            colorize_println(format!("cannot spawn runtime thread: {e}"), RedFg);
            ExitCode::FAILURE
        }
    }
}

/// Drive `fut` on the tokio runtime `#[tokio::main]` used to generate.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("cannot build tokio runtime")
        .block_on(fut)
}

async fn run(mut args: Args) -> ExitCode {
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
        if let Some(ref sa) = args.vm_stack_arena {
            v.push(format!("--vm-stack-arena={}", sa));
        }
        if let Some(ref sl) = args.vm_stack_limit {
            v.push(format!("--vm-stack-limit={}", sl));
        }
        if let Some(ref so) = args.thread_stack_limit {
            v.push(format!("--thread-stack-limit={}", so));
        }
        if let Some(ref ts) = args.thread_stack {
            v.push(format!("--thread-stack={}", ts));
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

    // vm-stack-arena (default: 256k)
    if let Some(ref ss) = args.vm_stack_arena {
        if let Some(size) = parse_byte_size(ss) {
            if rt.set_vm_stack_arena(size).is_err() {
                colorize_println(
                    format!("cannot allocate vm-stack-arena of size {ss}"),
                    RedFg,
                );
                std::process::exit(-1);
            }
        } else {
            colorize_println(format!("invalid vm-stack-arena: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    // vm-stack-limit (default: 512k)
    if let Some(ref ss) = args.vm_stack_limit {
        if let Some(size) = parse_byte_size(ss) {
            rt.set_vm_stack_limit(size);
        } else {
            colorize_println(format!("invalid vm-stack-limit: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    // thread-stack-limit: omit/0 = auto mode (from thread stack bottom)
    if let Some(ref ss) = args.thread_stack_limit {
        if let Some(size) = parse_byte_size(ss) {
            rt.set_thread_stack_limit(size);
        } else {
            colorize_println(format!("invalid thread-stack-limit: {ss}"), RedFg);
            std::process::exit(-1);
        }
    } else if !zuqe::Runtime::CAN_AUTO_THREAD_STACK_LIMIT {
        // Platform can't report the stack bottom: auto mode would leave no
        // C-stack guard, so fall back to a strict 512 KiB limit.
        rt.set_thread_stack_limit(512 << 10);
    }

    rt.set_module_normalizer(module_loader::js_module_normalizer);
    rt.set_module_loader(module_loader::js_module_loader);

    crate::aike::register_runtime_traces(&mut rt);

    // create context and setup
    let mut ctx = rt
        .create_context(move |cx| {
            ScopedContext::ECMA_CONTEXT_SETUP(cx)?;

            // use classes
            cx.use_classes(&[
                JSEventEmitter::class_id(),
                JSStdioHandle::class_id(),
                JSCronJob::class_id(),
            ])?;

            // initialize native modules
            crate::builtin_modules::install_all(cx)?;

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
    let eval_name: zuqe::EvalName;
    let is_module: bool;
    // The CommonJS entry module's absolute path, when the entry runs as CJS.
    let cjs_entry: Option<String>;

    if let Some(ref_filename) = filename {
        //
        // Run script file
        //
        let (script_bytes, script_path) = match module_loader::read_script(&ref_filename) {
            Ok((buf, name)) => (buf, name),
            Err(e) => {
                colorize_println(e, RedFg);
                std::process::exit(-1);
            }
        };
        let script_str = String::from_utf8_lossy(&script_bytes).to_string();

        // A TypeScript entry is classified by SWC, which sees `import type`
        // / `export type` that a post-transpile scan would no longer see.
        // Everything else is classified from the source text.
        let (transpiled, is_ts_module): (String, bool) = if transpile::needs_transpile(&script_path)
        {
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
            (script_str, false)
        };

        // Convert the entry filename to a canonical `file://` URL for module resolution.
        // Module names (and `import.meta.url`) are URLs, and a `file://` base is also
        // required so that dynamic `import()` resolves relative specifiers correctly.
        let url =
            zuqe::path_to_url(&ref_filename).expect("entry path must convert to a file:// URL");

        // `-m` forces a module; otherwise `.mjs` / `.mts` are always modules
        // and every other file follows its own source, which makes a
        // non-module file CommonJS (Node's rule).
        let is_module_value = args.module
            || is_ts_module
            || matches!(cjs::format_of(&script_path, &transpiled), cjs::Format::Esm);
        cjs_entry = (!is_module_value).then(|| {
            zuqe::url_to_abspath(&url)
                .unwrap_or_else(|| std::path::PathBuf::from(&script_path))
                .to_string_lossy()
                .into_owned()
        });

        script = transpiled;
        eval_name = zuqe::EvalName::BaseUrl(url);
        is_module = is_module_value;
    } else if let Some(eval_code) = args.eval {
        //
        // Eval mode: inline script
        //
        // Without `-m`, `-e` code runs as CommonJS, matching Node.
        cjs_entry = (!args.module).then(|| "[eval]".to_string());
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

    let flags = if args.no_await_result {
        EvalFlags::NO_AWAIT_RESULT
    } else {
        EvalFlags::empty()
    };

    let result = match cjs_entry {
        // CommonJS entry: the module body runs synchronously inside
        // `cjs::run_entry`, then an empty script drives the event loop to
        // completion — the job `eval_wait` would otherwise have done.
        Some(entry) => {
            let entered = ctx.with_root_scope(|mut cx| cjs::run_entry(&mut cx, &entry, &script));

            match entered {
                Ok(()) => {
                    ctx.eval_wait(
                        b"",
                        &zuqe::EvalName::Name(String::new()),
                        EvalType::Global,
                        flags,
                    )
                    .await
                }
                Err(_) => ExitStatus::Exception(ctx.take_exception()),
            }
        }
        None => {
            ctx.eval_wait(
                script.as_bytes(),
                &eval_name,
                if is_module {
                    EvalType::Module
                } else {
                    EvalType::Global
                },
                flags,
            )
            .await
        }
    };

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

    // Kill `abortOnParentExit` children synchronously: every script exit
    // (normal end, process.exit, engine error) funnels through here.
    // std::process::exit maps to ExitProcess on Windows, which skips CRT
    // atexit entirely, so this must happen explicitly before it.
    crate::aike::kill_all_children();
    crate::mcp::kill_all_mcp_children();

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
