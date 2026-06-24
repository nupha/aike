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
mod llm;
mod output;
mod path;
mod repl;
mod request;
mod responses;
mod whatwg;
mod worker;

use std::{io::Write, process::ExitCode};

use clap::{CommandFactory, Parser};
use color::{Color::*, colorize_println};
use linkme::distributed_slice;
use zuqe::{
    ClassID, EvalFlags, EvalType, Exception, ExitStatus, ModuleDef, NativeObject, Runtime,
    ScopedContext, builtins::promise::PromiseFuture,
};

use crate::{
    aike::{JSCronJob, JSFileHandle, JSStdioHandle, gc_trace_cron, gc_trace_io},
    browser::{JSBrowser, JSTab, init_mod_browser},
    class_event_emitter::JSEventEmitter,
    fs::init_mod_fs,
    global::setup_globals,
    html::{HtmlDocument, HtmlElement, init_mod_html},
    http::{
        JSHttpRequest, JSHttpResponse, JSHttpServer, JSMcpStdio, gc_trace_stdio_state,
        init_mod_http,
    },
    llm::{JSEngine, JSLlmClient, JSLlmContext, JSLlmModel, JSSamplerChain, init_mod_llm},
    path::init_mod_path,
    request::JSResponse,
    whatwg::{
        abort::{JSAbortController, JSAbortSignal},
        text_decoder::JSTextDecoder,
        text_encoder::JSTextEncoder,
        timeout::gc_trace_timeout,
        url::{JSURL, JSURLSearchParams},
    },
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

    #[arg(short, long, default_value_t = false, help = "load as esm module")]
    module: bool,

    #[arg(
        long,
        default_value_t = String::from("1m"),
        value_name = "n",
        help = "set memory limit to n"
    )]
    memory_limit: String,

    #[arg(
        long,
        default_value_t = String::from("1m"),
        value_name = "n",
        help = "set stack size to n"
    )]
    stack_size: String,

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
        eprintln!("[INFO] .env not loaded: {e}");
    }

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

    // --help without filename: show aike help and exit
    // --help with filename: pass through to script
    if args.help && args.filename.is_none() {
        let _ = Args::command().print_help();
        return ExitCode::SUCCESS;
    }
    if args.help {
        args.script_args.insert(0, "--help".to_string());
    }

    let filename: Option<String> = args.filename;

    // Build process.argv and process.execArgv
    let exe_path = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| p.to_string_lossy().to_string())
        .expect("failed to resolve current executable path");

    let argv: Vec<String> = {
        let mut v = Vec::with_capacity(2 + args.script_args.len());
        v.push(exe_path.clone());
        match &filename {
            Some(f) => v.push(f.clone()),
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
        v.push(format!("--memory-limit={}", args.memory_limit));
        v.push(format!("--stack-size={}", args.stack_size));
        v
    };

    // Create runtime
    let mut rt = Runtime::create().unwrap();

    // In release build, when set panic="abort" in Cargo.toml,
    // should install panic handler to ensure gc heap deallocate when panic.
    rt.install_panic_handler();

    // memory-limit
    {
        let ss = args.memory_limit;
        if let Some(size) = parse_byte_size(&ss) {
            rt.gc_set_memory_limit(size);
        } else {
            colorize_println(format!("invalid memory-limit: {ss}"), RedFg);
            std::process::exit(-1);
        }
    }

    // stack-size
    {
        let ss = args.stack_size;
        if let Some(size) = parse_byte_size(&ss) {
            rt.set_stack_size(size);
        } else {
            colorize_println(format!("invalid stack-size: {ss}"), RedFg);
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
                JSLlmModel::class_id(),
                JSLlmContext::class_id(),
                JSEngine::class_id(),
                JSLlmClient::class_id(),
                JSSamplerChain::class_id(),
                JSStdioHandle::class_id(),
                JSFileHandle::class_id(),
                JSResponse::class_id(),
                JSTextDecoder::class_id(),
                JSTextEncoder::class_id(),
                JSURL::class_id(),
                JSURLSearchParams::class_id(),
                JSAbortController::class_id(),
                JSAbortSignal::class_id(),
                JSCronJob::class_id(),
                JSHttpServer::class_id(),
                JSMcpStdio::class_id(),
                JSHttpRequest::class_id(),
                JSHttpResponse::class_id(),
                HtmlDocument::class_id(),
                HtmlElement::class_id(),
                JSBrowser::class_id(),
                JSTab::class_id(),
            ])?;

            // initialize native modules
            init_mod_llm(cx)?;
            init_mod_fs(cx)?;
            init_mod_http(cx)?;
            init_mod_path(cx)?;
            init_mod_html(cx)?;
            init_mod_browser(cx)?;

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

    //
    // REPL mode: run repl task and exit.
    //
    let Some(ref_filename) = filename else {
        repl::run(rt, ctx).await;
        return ExitCode::SUCCESS;
    };

    //
    // Run script file mode
    //
    let script = match std::fs::read_to_string(&ref_filename) {
        Ok(s) => s,
        Err(e) => {
            colorize_println(format!("cannot load '{ref_filename}': {e}"), RedFg);
            std::process::exit(-1);
        }
    };

    let is_module = args.module
        || ref_filename.ends_with(".mjs")
        || ModuleDef::detect_module(script.as_bytes());

    let mut promise_future: Option<PromiseFuture> = None;

    let res = ctx.with_new_scope(|cx| {
        let r = cx
            .eval(
                script.as_bytes(),
                ref_filename.as_str(),
                if is_module {
                    EvalType::Module
                } else {
                    EvalType::Global
                },
                EvalFlags::empty(),
            )
            .map(|v| v.as_object_Promise().copied());

        match r {
            Ok(Some(p)) => match p.into_future(cx) {
                Ok(fut) => {
                    promise_future = Some(fut);
                    Ok(())
                }
                Err(_) => Err(-1),
            },
            Ok(None) => Ok(()),
            Err(_) => Err(-1),
        }
    });

    if res.is_err() {
        let exit_code = ctx.with_root_scope(|mut cx0| {
            let code: u8 = match cx0.take_exception() {
                Exception::Error(e) => {
                    colorize_println(e.to_string(&mut cx0).unwrap().encode_utf8(false), GreenFg);

                    // value 'e' is in cx0 (top-level scope), it won't be gc collected.
                    // we should remove it from protection manually.
                    if let Some(n) = e.gc_node_ptr() {
                        cx0.remove_local(n);
                    }

                    1
                }
                Exception::OutOfMemory => {
                    colorize_println("out of memory", RedFg);
                    1
                }
                Exception::No => {
                    #[cfg(debug_assertions)]
                    unreachable!();
                    #[cfg(not(debug_assertions))]
                    0
                }
                Exception::TooManyClasses => {
                    unreachable!();
                }
            };
            code
        });
        return ExitCode::from(exit_code);
    }

    // await till all background jobs are finished
    let result = if let Some(fut) = promise_future {
        let (rt_res, promise_res) = tokio::join!(rt.run_async(), fut);

        match promise_res {
            Ok(result) => {
                // suppress eval result printing
                let _ = result;
            }
            Err(reason) => {
                // Promise reject
                let err_msg = ctx.with_new_scope(|cx| {
                    let s = reason.to_string(cx).unwrap();
                    s.encode_utf8(false).to_string()
                });
                colorize_println(err_msg, RedFg);
            }
        }

        rt_res
    } else {
        rt.run_async().await
    };

    match result {
        ExitStatus::Value(_) => ExitCode::SUCCESS,
        ExitStatus::Interrupted(code) => {
            #[cfg(debug_assertions)]
            log::debug!("Process interrupted with code {:?}", code);
            ExitCode::from(code)
        }
        ExitStatus::Exception(Exception::OutOfMemory) => {
            colorize_println("out of memory", RedFg);
            ExitCode::FAILURE
        }
        ExitStatus::Exception(Exception::Error(err)) => {
            ctx.with_new_scope(|cx| {
                colorize_println(err.to_string(cx).unwrap().encode_utf8(false), RedFg);
            });
            ExitCode::FAILURE
        }
        ExitStatus::Exception(Exception::No | Exception::TooManyClasses) => {
            unreachable!();
        }
    }
}

fn js_module_loader(
    ctx: &mut ScopedContext,
    file_name: &str,
    _: *mut std::ffi::c_void,
) -> Result<ModuleDef, i32> {
    if file_name.ends_with(".dylib") || file_name.ends_with(".so") || file_name.ends_with(".dll") {
        ctx.throw_internal_error("dynamic library loading not implemented");
        Err(-1)
    } else {
        // js file
        match std::fs::read(file_name) {
            Ok(buf) => ctx.compile_module(&buf, file_name, false),
            Err(e) => {
                ctx.throw_reference_error(&format!("could not load module '{file_name}'. {e}"));
                Err(-1)
            }
        }
    }
}
