// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    process::{Command, ExitCode, ExitStatus, Stdio},
};

use zuqe::{
    Array, CallContext, FieldDef, JS_GPN_ENUM_ONLY, JS_GPN_INDEX_MASK, JS_PROP_C_W_E, JSResult,
    JSValue, Object, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::{
    class_event_emitter::create_event_emitter_instance, fs::class_file_handle::JSFileHandle,
};

/// Build `process` object exposing system info.
/// The result is a proper EventEmitter instance so `process.on/emit/once`
/// work natively (matching Node.js/Bun behaviour).
pub(crate) fn build_process_object(
    ctx: &mut ScopedContext,
    argv: &[String],
    exec_argv: &[String],
) -> Result<Object, i32> {
    let mut process = create_event_emitter_instance(ctx)?;

    // argv[0] = executable path, argv[1] = script path, argv[2+] = script args
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(argv.len());
        for a in argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(s.into());
        }
        let arr = Array::from_values(ctx, elems)?;
        process.define_property_value_str(ctx, "argv", arr.into(), JS_PROP_C_W_E)?;
    }

    // execArgv — runtime-specific flags passed before the script path
    {
        let mut elems: Vec<JSValue> = Vec::with_capacity(exec_argv.len());
        for a in exec_argv {
            let s = ctx.create_string_from_utf8(a.as_bytes().to_vec())?;
            elems.push(s.into());
        }
        let arr = Array::from_values(ctx, elems)?;
        process.define_property_value_str(ctx, "execArgv", arr.into(), JS_PROP_C_W_E)?;
    }

    let entries = &[
        FieldDef::FUNC("cwd", 0, js_cwd, 0),
        FieldDef::FUNC("spawnSync", 0, js_spawn, 0),
        FieldDef::FUNC("spawn", 0, js_spawn, 1),
        FieldDef::FUNC("exit", 1, js_exit, 0),
        FieldDef::VALUE(
            "env",
            |cx| -> JSResult {
                let mut env_obj = cx.create_base_object()?;
                // 1) Load .env defaults (lower priority)
                for (key, value) in load_dotenv() {
                    let v = cx.create_string_from_utf8(value.as_bytes().to_vec())?;
                    env_obj.set_property_str(cx, &key, v.into())?;
                }
                // 2) Overlay real system env vars (higher priority), matching Bun behaviour
                for (key, value) in std::env::vars() {
                    let v = cx.create_string_from_utf8(value.as_bytes().to_vec())?;
                    env_obj.set_property_str(cx, &key, v.into())?;
                }
                Ok(env_obj.into())
            },
            JS_PROP_C_W_E,
        ),
        FieldDef::VALUE(
            "stdin",
            |cx| JSFileHandle::new_instance_stdin(cx).map(JSValue::Object),
            zuqe::JS_PROP_ENUMERABLE,
        ),
        FieldDef::VALUE(
            "stdout",
            |cx| JSFileHandle::new_instance_stdout(cx).map(JSValue::Object),
            zuqe::JS_PROP_ENUMERABLE,
        ),
        FieldDef::VALUE(
            "stderr",
            |cx| JSFileHandle::new_instance_stderr(cx).map(JSValue::Object),
            zuqe::JS_PROP_ENUMERABLE,
        ),
    ];

    process.install_fields(ctx, entries)?;

    Ok(process)
}

/// Load key=value pairs from `.env` in the current working directory.
///
/// Supports `#` comments, blank-line skips, and single/double-quoted values.
fn load_dotenv() -> Vec<(String, String)> {
    let path = Path::new(".env");
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut vars = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_string();
            if key.is_empty() {
                continue;
            }
            let mut val = v.trim().to_string();
            // Strip surrounding single/double quotes
            if val.len() >= 2 {
                let b = val.as_bytes();
                if (b[0] == b'"' && b[val.len() - 1] == b'"')
                    || (b[0] == b'\'' && b[val.len() - 1] == b'\'')
                {
                    val = val[1..val.len() - 1].to_string();
                }
            }
            vars.push((key, val));
        }
    }
    vars
}

fn js_cwd(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    match std::env::current_dir() {
        Ok(p) => ctx
            .create_string_from_utf8(p.to_string_lossy().as_bytes().to_vec())
            .map(JSValue::from),
        Err(e) => {
            ctx.throw_internal_error(&e.to_string());
            Err(-1)
        }
    }
}

fn js_exit(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let code = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(ctx).map(|n| n.to_i32()).unwrap_or(0)
    } else {
        0
    };
    ctx.rt_mut().interrupt(ExitCode::from(code as u8));
    Err(-1)
}

fn js_spawn(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_async = cc.magic() == 1;

    // arg 0: command string
    let cmd = cc.arg(0).to_string(ctx)?;
    let cmd_str = cmd.content().encode_utf8(false).to_string();

    // arg 1 (optional): args array
    let mut args: Vec<String> = Vec::new();
    if cc.length() > 1 {
        if let Some(arr) = cc.arg(1).as_object_Array() {
            for v in arr.payload().values() {
                if let Some(s) = v.as_string() {
                    args.push(s.content().encode_utf8(false).to_string());
                }
            }
        }
    }

    // Defaults: inherit stdin, pipe stdout/stderr
    let mut stdin_mode = Stdio::inherit();
    let mut stdout_mode = Stdio::piped();
    let mut stderr_mode = Stdio::piped();

    let mut cwd = None;
    let mut env_opt: Option<HashMap<String, String>> = None;

    // arg 2 (optional): options object
    if cc.length() > 2 {
        if let Some(opts) = cc.arg(2).as_object().copied() {
            if let Some(v) = get_str_prop(ctx, &opts, "cwd") {
                cwd = Some(v);
            }

            // env — replaces entire env when provided
            if let Some(env_map) = parse_env_obj(ctx, &opts)? {
                env_opt = Some(env_map);
            }

            // stdio modes
            if let Some(mode) = get_str_prop(ctx, &opts, "stdin") {
                match mode.as_str() {
                    "pipe" => stdin_mode = Stdio::piped(),
                    "inherit" => stdin_mode = Stdio::inherit(),
                    "ignore" => stdin_mode = Stdio::null(),
                    _ => {}
                }
            }
            if let Some(mode) = get_str_prop(ctx, &opts, "stdout") {
                match mode.as_str() {
                    "pipe" => stdout_mode = Stdio::piped(),
                    "inherit" => stdout_mode = Stdio::inherit(),
                    "ignore" => stdout_mode = Stdio::null(),
                    _ => {}
                }
            }
            if let Some(mode) = get_str_prop(ctx, &opts, "stderr") {
                match mode.as_str() {
                    "pipe" => stderr_mode = Stdio::piped(),
                    "inherit" => stderr_mode = Stdio::inherit(),
                    "ignore" => stderr_mode = Stdio::null(),
                    _ => {}
                }
            }
        }
    }

    let mut cmd_obj = Command::new(&cmd_str);
    cmd_obj.args(&args);
    cmd_obj.stdin(stdin_mode);
    cmd_obj.stdout(stdout_mode);
    cmd_obj.stderr(stderr_mode);

    if let Some(dir) = cwd {
        cmd_obj.current_dir(dir);
    }
    if let Some(env_map) = env_opt {
        cmd_obj.env_clear();
        cmd_obj.envs(env_map);
    }

    let subprocess = cmd_obj.spawn().map_err(|e| {
        ctx.throw_internal_error(&format!("spawn failed: {e}"));
        -1
    })?;

    let pid = subprocess.id() as i32;

    if is_async {
        spawn_async(ctx, subprocess, pid)
    } else {
        spawn_sync(ctx, subprocess, pid)
    }
}

fn spawn_sync(ctx: &mut ScopedContext, mut child: std::process::Child, pid: i32) -> JSResult {
    let mut stdout_buf = Vec::new();
    if let Some(ref mut r) = child.stdout {
        let _ = r.read_to_end(&mut stdout_buf);
    }
    let mut stderr_buf = Vec::new();
    if let Some(ref mut r) = child.stderr {
        let _ = r.read_to_end(&mut stderr_buf);
    }

    //
    // wait sub process exit
    //
    let exit_status = child.wait().map_err(|e| {
        ctx.throw_internal_error(&format!("wait failed: {e}"));
        -1
    })?;

    let exit_code = exit_status.code();
    let signal_code = get_signal_code(&exit_status);

    //
    // make_spawn_result(ctx, pid, &stdout_buf, &stderr_buf, &exit_status)
    //
    let mut result = ctx.create_base_object()?;
    let ta_out = TypedArray::create_from_vec_u8(ctx, stdout_buf)?;
    let ta_err = TypedArray::create_from_vec_u8(ctx, stderr_buf)?;
    let signal_str = signal_code.map_or(Ok(JSValue::NULL), |sig| {
        ctx.create_string_from_str(signal_name(sig))
            .map(JSValue::from)
    })?;

    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", ta_out.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", ta_err.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(
        ctx,
        "exitCode",
        exit_code.map_or(JSValue::NULL, JSValue::from),
        JS_PROP_C_W_E,
    )?;
    result.define_property_value_str(ctx, "signalCode", signal_str, JS_PROP_C_W_E)?;
    result.define_property_value_str(
        ctx,
        "success",
        JSValue::Bool(exit_code == Some(0) && signal_code.is_none()),
        JS_PROP_C_W_E,
    )?;

    Ok(result.into())
}

fn spawn_async(ctx: &mut ScopedContext, child: std::process::Child, pid: i32) -> JSResult {
    let (prom_stdout, tx_stdout) = Promise::create_with_channel::<Result<Vec<u8>, String>>(
        ctx,
        |cx, data: Result<Vec<u8>, String>| -> JSResult {
            match data {
                Ok(buf) => TypedArray::create_from_vec_u8(cx, buf).map(JSValue::from),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        },
    )?;

    let (prom_stderr, tx_stderr) =
        Promise::create_with_channel::<Result<Vec<u8>, String>>(ctx, |cx, data| -> JSResult {
            match data {
                Ok(buf) => TypedArray::create_from_vec_u8(cx, buf).map(JSValue::from),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;

    let (prom_exit, tx_exit) =
        Promise::create_with_channel::<Result<i32, String>>(ctx, |cx, data| -> JSResult {
            match data {
                Ok(code) => Ok(JSValue::from(code)),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        })?;

    let (prom_signal, tx_signal) = Promise::create_with_channel::<Result<Option<String>, String>>(
        ctx,
        |cx, data| -> JSResult {
            match data {
                Ok(sig) => match sig {
                    Some(name) => cx
                        .create_string_from_utf8(name.into_bytes())
                        .map(JSValue::from),
                    None => Ok(JSValue::NULL),
                },
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        },
    )?;

    // Build result object with Promises
    let mut result = ctx.create_base_object()?;
    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", prom_stdout.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", prom_stderr.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", prom_exit.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", prom_signal.into(), JS_PROP_C_W_E)?;

    // Run IO + wait on tokio's blocking thread pool.
    // Child is spawned synchronously above (fast, non-blocking) so pid is
    // available immediately.  Three parallel spawn_blocking tasks read
    // stdout / stderr / wait concurrently, avoiding the classic pipe
    // deadlock that occurs when one pipe fills while the other is never
    // drained.
    tokio::spawn(async move {
        // Wrap child in Option so we can borrow fields via &mut (not move)
        // and avoid partial-move when taking pipes.
        let mut child_opt = Some(child);

        let stdout_handle = {
            let mut pipe = child_opt.as_mut().unwrap().stdout.take();
            tokio::task::spawn_blocking(move || {
                let mut buf = Vec::new();
                if let Some(ref mut r) = pipe {
                    let _ = r.read_to_end(&mut buf);
                }
                buf
            })
        };

        let stderr_handle = {
            let mut pipe = child_opt.as_mut().unwrap().stderr.take();
            tokio::task::spawn_blocking(move || {
                let mut buf = Vec::new();
                if let Some(ref mut r) = pipe {
                    let _ = r.read_to_end(&mut buf);
                }
                buf
            })
        };

        let wait_handle = tokio::task::spawn_blocking(move || {
            let mut child = child_opt
                .take()
                .expect("child_opt should be Some after pipes taken");
            child.wait()
        });

        // Run all three blocking tasks in parallel
        let (out_res, err_res, wait_res) = tokio::join!(stdout_handle, stderr_handle, wait_handle);

        let stdout_buf = out_res.unwrap_or_default();
        let stderr_buf = err_res.unwrap_or_default();
        let _ = tx_stdout.send(Ok(stdout_buf));
        let _ = tx_stderr.send(Ok(stderr_buf));

        match wait_res {
            Ok(Ok(status)) => {
                let code = status.code().unwrap_or(-1);
                let _ = tx_exit.send(Ok(code));
                let sig = get_signal_code(&status).map(signal_name).map(String::from);
                let _ = tx_signal.send(Ok(sig));
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                let _ = tx_exit.send(Err(msg.clone()));
                let _ = tx_signal.send(Err(msg));
            }
            Err(e) => {
                let msg = format!("wait panicked: {e}");
                let _ = tx_exit.send(Err(msg.clone()));
                let _ = tx_signal.send(Err(msg));
            }
        }
    });

    Ok(result.into())
}

/// Extract a string property from an options object; returns None if missing.
fn get_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<String> {
    let v = obj.get_property_str(ctx, key).ok()?;
    let s = v.as_string()?;
    Some(s.content().encode_utf8(false).to_string())
}

/// Parse the `env` option: iterate the JS object and collect into a HashMap.
/// Returns `Some(map)` when env is provided, `None` when not set.
fn parse_env_obj(
    ctx: &mut ScopedContext,
    opts: &Object,
) -> Result<Option<HashMap<String, String>>, i32> {
    let v = opts.get_property_str(ctx, "env")?;
    if v.is_undefined_or_null() {
        return Ok(None);
    }
    let Some(mut env_obj) = v.as_object().copied() else {
        return Ok(None);
    };

    let mut map = HashMap::new();
    let pks = env_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_INDEX_MASK)?;
    for pk in pks.iter() {
        let key_v = pk.atom.to_value(ctx)?;
        let key_r = key_v.to_string(ctx)?;
        let key = key_r.encode_utf8(false).to_string();

        let val = env_obj.get_property(ctx, pk.atom)?;
        if let Some(s) = val.as_string() {
            map.insert(key, s.content().encode_utf8(false).to_string());
        }
    }
    Ok(Some(map))
}

/// Get signal number (Unix-only).
#[cfg(unix)]
fn get_signal_code(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}
#[cfg(not(unix))]
fn get_signal_code(_status: &ExitStatus) -> Option<i32> {
    None
}

/// Map signal number to POSIX signal name (e.g. 9 → "SIGKILL").
fn signal_name(sig: i32) -> &'static str {
    match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        _ => "UNKNOWN",
    }
}
