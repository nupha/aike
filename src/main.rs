// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#![allow(non_snake_case)]

mod aike;
mod class_event_emitter;
mod class_llama_client;
mod class_llama_client_responses;
mod class_llama_context;
mod class_llama_engine;
mod class_llama_model;
mod class_llama_sampler;
mod class_text_decoder;
mod engine;
mod fs;
mod global;
mod helpers;
mod mod_llama;
mod output;
mod request;
mod responses;
mod serve;
mod timeout;
mod worker;

use std::{io::Write, process::ExitCode};

use clap::Parser;
use colorized::{Colors::*, colorize_println};
use linkme::distributed_slice;
use zuqe::{
    ClassID, Context, EvalFlags, EvalType, Exception, ModuleDef, NativeObject, Runtime,
    ScopedContext, builtins::promise::PromiseFuture,
};

use crate::{
    aike::{JSIoHandle, gc_trace_io},
    class_event_emitter::JSEventEmitter,
    class_llama_client::JSLlamaClient,
    class_llama_context::JSLlamaContext,
    class_llama_engine::JSEngine,
    class_llama_model::JSLlamaModel,
    class_llama_sampler::JSSamplerChain,
    class_text_decoder::JSTextDecoder,
    fs::init_mod_fs,
    global::setup_globals,
    mod_llama::init_mod_llama,
    request::JSResponse,
    timeout::gc_trace_timeout,
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
#[command(version)]
struct Args {
    #[arg(short, long, default_value_t = false, help = "load as ES6 module")]
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

    #[arg(help = "file to execute")]
    filename: String,

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

pub(crate) fn get_llm_base_url() -> String {
    std::env::var("AIKE_LLM_BASE_URL").unwrap_or_default()
}
pub(crate) fn get_llm_api_key() -> String {
    std::env::var("AIKE_LLM_API_KEY").unwrap_or_default()
}
pub(crate) fn get_llm_model() -> String {
    std::env::var("AIKE_LLM_MODEL").unwrap_or_default()
}
pub(crate) fn get_sandbox_path() -> String {
    std::env::var("AIKE_SANDBOX_PATH").unwrap_or_default()
}

#[tokio::main]
async fn main() -> ExitCode {
    dotenvy::dotenv().ok();

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
            .filter_level(log::LevelFilter::Trace)
            //.filter_level(log::LevelFilter::Debug)
            .target(env_logger::Target::Pipe(Box::new(tee)))
            .init();
    }

    let args = Args::parse();

    // Build process.argv and process.execArgv
    let exe_path = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "zjs".to_string());

    let argv: Vec<String> = {
        let mut v = Vec::with_capacity(2 + args.script_args.len());
        v.push(exe_path);
        v.push(args.filename.clone());
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

    let filename = args.filename;
    let script = match std::fs::read_to_string(&filename) {
        Ok(s) => s,
        Err(e) => {
            colorize_println(format!("can't load file: {e}"), RedFg);
            std::process::exit(-1);
        }
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
        gc_trace_io(gcx)
    });

    // register native classes
    for class_reg_fn in USER_CLASS_DEFS {
        class_reg_fn(&mut rt).unwrap();
    }

    let is_module =
        args.module || filename.ends_with(".mjs") || ModuleDef::detect_module(script.as_bytes());

    // create context and setup
    let mut ctx = rt
        .create_context(|cx| {
            Context::ECMA_CONTEXT_SETUP(cx)?;

            // use classes
            cx.use_classes(&[
                JSEventEmitter::class_id(),
                JSLlamaModel::class_id(),
                JSLlamaContext::class_id(),
                JSEngine::class_id(),
                JSLlamaClient::class_id(),
                JSSamplerChain::class_id(),
                JSIoHandle::class_id(),
                JSResponse::class_id(),
                JSTextDecoder::class_id(),
            ])?;

            // register native modules
            init_mod_llama(cx)?;
            init_mod_fs(cx)?;

            setup_globals(cx, &argv, &exec_argv)
        })
        .inspect_err(|_| {
            let exc = rt.take_exception();
            let _ = rt.create_context(|cx| {
                let t = exc.to_text(cx);
                println!("{:?}", t);
                Ok(())
            });
        })
        .unwrap();

    #[cfg(debug_assertions)]
    debug_assert_eq!(ctx.gc_heap().scope_max_depth(), 0, "no scope now");

    let mut promise_future: Option<PromiseFuture> = None;

    let res = ctx.with_new_scope(|mut scope| -> Result<(), i32> {
        let v = scope.eval(
            script.as_bytes(),
            &filename,
            if is_module {
                EvalType::Module
            } else {
                EvalType::Global
            },
            EvalFlags::empty(),
        )?;

        if let Some(p) = v.as_object_Promise() {
            // eval result is a Promise
            promise_future = Some((*p).into_future(&mut scope)?);
        } else {
            // suppress eval result printing — it leaks "undefined" to child stdout
            let _ = v;
        }
        Ok(())
    });

    #[cfg(debug_assertions)]
    debug_assert_eq!(ctx.gc_heap().scope_max_depth(), 0);

    if res.is_err() {
        // eval error
        return match rt.take_exception() {
            Exception::Interrupted(code) => code,
            Exception::Error(e) => ctx.with_new_scope(|mut cx| {
                colorize_println(e.to_string(&mut cx).unwrap().encode_utf8(false), GreenFg);
                ExitCode::FAILURE
            }),
            Exception::OutOfMemory => {
                colorize_println("out of memory", RedFg);
                ExitCode::FAILURE
            }
            Exception::No | Exception::TooManyClasses => {
                unreachable!();
            }
        };
    }

    // await till all background jobs are finished
    let run_result = if let Some(js_fut) = promise_future {
        let (rt_res, promise_res) = tokio::join!(rt.run_async(), js_fut);

        match promise_res {
            Ok(result) => {
                // suppress eval result printing
                let _ = result;
            }
            Err(reason) => {
                // Promise reject
                let err_msg = ctx.with_new_scope(|mut cx| {
                    let s = reason.to_string(&mut cx).unwrap();
                    s.encode_utf8(false).to_string()
                });
                colorize_println(err_msg, RedFg);
            }
        }

        rt_res
    } else {
        rt.run_async().await
    };

    match run_result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Exception::OutOfMemory) => {
            colorize_println("out of memory", RedFg);
            ExitCode::FAILURE
        }
        Err(Exception::Interrupted(code)) => code,
        Err(Exception::Error(err)) => {
            ctx.with_new_scope(|mut cx| {
                colorize_println(err.to_string(&mut cx).unwrap().encode_utf8(false), RedFg);
            });
            ExitCode::FAILURE
        }
        Err(Exception::No | Exception::TooManyClasses) => {
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
        // let lib = unsafe { Library::new(file_name) }.map_err(|_| {
        //     ctx.throw_reference_error(&format!(
        //         "could not load module filename '{file_name}' as shared library"
        //     ));
        //     -1
        // })?;

        // unsafe {
        //     let init_fn = match lib.get::<ZqInitModuleFn>(c"js_init_module") {
        //         Ok(sym) => sym,
        //         Err(_) => {
        //             ctx.throw_reference_error(&format!(
        //                 "could not load module filename '{file_name}': js_init_module not found"
        //             ));
        //             return Err(-1);
        //         }
        //     };

        //     let ctx_ref: ZqContextRef = ctx as *mut ScopedContext as ZqContextRef;
        //     let m: ZqModuleRef = init_fn(ctx_ref, CString::new(file_name).unwrap().as_ptr());
        //     if m.is_null() {
        //         log::error!("could not load library '{file_name}': initialization error");
        //         Err(-1)
        //     } else {
        //         ctx.add_shared_library(lib);
        //         Ok(unsafe { *Box::from_raw(m as *mut ModuleDef) })
        //     }
        // }
        todo!()
    } else {
        // js file
        match std::fs::read(file_name) {
            Ok(buf) => ctx.compile_module(&buf, file_name, false),
            Err(_) => {
                ctx.throw_reference_error(&format!("could not load module '{file_name}'"));
                Err(-1)
            }
        }
    }
}
