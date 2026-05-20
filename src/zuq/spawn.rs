// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! spawn.rs — spawnSync / spawn (async) child process support

use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{ExitStatus, Stdio},
};

use zuqe::{
    CallContext, Callback, JS_GPN_ENUM_ONLY, JS_GPN_INDEX_MASK, JS_PROP_C_W_E, JS_PROP_ENUMERABLE,
    JSResult, JSValue, Object, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use crate::fs::class_file_handle::JSFileHandle;

pub(super) fn js_spawn(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
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

    // input option (sync only): string / TypedArray / ArrayBuffer → bytes to write to stdin
    let mut input_data: Option<Vec<u8>> = None;

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

            // input — extract bytes for spawnSync stdin
            if !is_async {
                let input_val = opts
                    .get_property_str(ctx, "input")
                    .unwrap_or(JSValue::UNDEFINED);
                if !input_val.is_undefined_or_null() {
                    if let Some(s) = input_val.as_string() {
                        input_data = Some(s.content().encode_utf8(false).into_owned().into_bytes());
                    } else if let Some(obj) = input_val.as_object() {
                        let payload = obj.payload();
                        if let Some(ta) = payload.typed_array() {
                            input_data = Some(ta.bytes().to_vec());
                        } else if let Some(ab) = payload.array_buffer() {
                            input_data = Some(ab.bytes().to_vec());
                        }
                    }
                }
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

    // input implies stdin=pipe
    if input_data.is_some() {
        stdin_mode = Stdio::piped();
    }

    if is_async {
        let mut cmd_obj = tokio::process::Command::new(&cmd_str);
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

        let pid = subprocess.id().map(|id| id as i32).unwrap_or(-1);
        spawn_async(ctx, subprocess, pid)
    } else {
        let mut cmd_obj = std::process::Command::new(&cmd_str);
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
        spawn_sync(ctx, subprocess, pid, input_data)
    }
}

fn spawn_sync(
    ctx: &mut ScopedContext,
    mut child: std::process::Child,
    pid: i32,
    input: Option<Vec<u8>>,
) -> JSResult {
    // Write input to stdin then close it, before reading output.
    // This avoids deadlock when the child waits for stdin before producing output.
    if let Some(data) = input {
        if let Some(ref mut stdin) = child.stdin {
            let _ = stdin.write_all(&data);
            let _ = stdin.flush();
        }
    }
    // Always close stdin (drop the pipe) so the child sees EOF
    child.stdin.take();

    let mut stdout_buf = Vec::new();
    if let Some(ref mut r) = child.stdout {
        let _ = r.read_to_end(&mut stdout_buf);
    }
    let mut stderr_buf = Vec::new();
    if let Some(ref mut r) = child.stderr {
        let _ = r.read_to_end(&mut stderr_buf);
    }

    // wait sub process exit
    let exit_status = child.wait().map_err(|e| {
        ctx.throw_internal_error(&format!("wait failed: {e}"));
        -1
    })?;

    let exit_code = exit_status.code();
    let signal_code = get_signal_code(&exit_status);

    let mut result = ctx.create_base_object()?;
    let ta_out = TypedArray::create_from_vec_u8(ctx, stdout_buf)?;
    let ta_err = TypedArray::create_from_vec_u8(ctx, stderr_buf)?;
    let signal_str = signal_code.map_or(Ok(JSValue::NULL), |sig| {
        ctx.create_string_from_str(crate::zuq::signal::signal_name(sig as u32).unwrap_or("UNKNOWN"))
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

fn spawn_async(ctx: &mut ScopedContext, mut child: tokio::process::Child, pid: i32) -> JSResult {
    // Take stdin pipe before moving child into async task,
    // so we can create a FileHandle wrapper while ScopedContext is available.
    // When stdin is not piped, this will be None → null.
    let stdin_val = match child.stdin.take() {
        Some(child_stdin) => {
            #[cfg(unix)]
            {
                // tokio::process::ChildStdin doesn't implement IntoRawFd, so we
                // borrow the fd via AsRawFd and dup() it, then let ChildStdin close
                // its own copy when dropped.
                use std::os::fd::{AsRawFd, FromRawFd};

                let raw_fd = child_stdin.as_raw_fd();
                let dup_fd = unsafe { libc::dup(raw_fd) };
                if dup_fd < 0 {
                    JSValue::NULL
                } else {
                    let std_file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
                    let tokio_file = tokio::fs::File::from_std(std_file);
                    // drop child_stdin (closes the original tokio-managed fd)
                    drop(child_stdin);
                    match JSFileHandle::new_instance(ctx, JSValue::UNDEFINED, tokio_file) {
                        Ok(obj) => JSValue::Object(obj),
                        Err(_) => JSValue::NULL,
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child_stdin;
                JSValue::NULL
            }
        }
        None => JSValue::NULL,
    };

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
                Ok(Some(name)) => cx
                    .create_string_from_utf8(name.into_bytes())
                    .map(JSValue::from),
                Ok(None) => Ok(JSValue::NULL),
                Err(e) => {
                    cx.throw_internal_error(&e);
                    Err(-1)
                }
            }
        },
    )?;

    // Build result object with Promises
    let mut result = ctx.create_base_object()?;

    result.define_property_value_str(ctx, "pid", JSValue::Int(pid), JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdin", stdin_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", prom_stdout.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", prom_stderr.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", prom_exit.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", prom_signal.into(), JS_PROP_C_W_E)?;

    // kill(signal?) — send signal to child process, default SIGTERM
    let kill_fn = ctx.create_native_function(Callback::Func(js_child_kill), "kill", 1, 0)?;
    result.define_property_value_str(ctx, "kill", kill_fn.into(), JS_PROP_C_W_E)?;

    // isRunning() — check if child process is still alive
    let is_running_fn =
        ctx.create_native_function(Callback::Func(js_child_isRunning), "isRunning", 0, 0)?;
    result.define_property_value_str(ctx, "isRunning", is_running_fn.into(), JS_PROP_C_W_E)?;

    // Spawn a pure async task — use tokio::process::Child's async IO directly.
    // Read stdout/stderr concurrently via tokio::join! to avoid pipe deadlock.
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;

        // Take pipes before join to avoid borrowing child across concurrent futures.
        let mut child_stdout = child.stdout.take();
        let mut child_stderr = child.stderr.take();

        let (stdout_buf, stderr_buf) = tokio::join!(
            async {
                let mut buf = Vec::new();
                if let Some(ref mut r) = child_stdout {
                    let _ = r.read_to_end(&mut buf).await;
                }
                buf
            },
            async {
                let mut buf = Vec::new();
                if let Some(ref mut r) = child_stderr {
                    let _ = r.read_to_end(&mut buf).await;
                }
                buf
            },
        );

        let _ = tx_stdout.send(Ok(stdout_buf));
        let _ = tx_stderr.send(Ok(stderr_buf));

        match child.wait().await {
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                let _ = tx_exit.send(Ok(code));
                let sig = get_signal_code(&status)
                    .and_then(|n| crate::zuq::signal::signal_name(n as u32))
                    .map(String::from);
                let _ = tx_signal.send(Ok(sig));
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = tx_exit.send(Err(msg.clone()));
                let _ = tx_signal.send(Err(msg));
            }
        }
    });

    Ok(result.into())
}

/// kill(signal?) — send signal to child process.
/// Reads `pid` from `this`, sends signal via libc::kill().
fn js_child_kill(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::Bool(false));
    }

    let sig = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        let arg = cc.arg(0);
        if let Some(s) = arg.as_string() {
            let name = s.content().encode_utf8(false);
            crate::zuq::signal::signal_number(&name)
                .map(|n| n as i32)
                .unwrap_or(libc::SIGTERM)
        } else {
            arg.to_number(ctx)?.to_i32()
        }
    } else {
        libc::SIGTERM
    };

    let ret = unsafe { libc::kill(pid, sig) };
    Ok(JSValue::Bool(ret == 0))
}

/// isRunning() — check if child process is still alive using kill(pid, 0).
fn js_child_isRunning(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::Bool(false));
    }
    let running = unsafe { libc::kill(pid, 0) == 0 };
    Ok(JSValue::Bool(running))
}

/// Read `pid` from the `this` object's "pid" property.
fn pid_from_this(ctx: &mut ScopedContext, cc: &CallContext) -> i32 {
    let this = cc.this();
    this.as_object()
        .and_then(|obj| obj.get_property_str(ctx, "pid").ok())
        .and_then(|v| v.to_number(ctx).ok().map(|n| n.to_i32()))
        .unwrap_or(-1)
}

/// Extract a string property from an options object; returns None if missing.
fn get_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Option<String> {
    let v = obj.get_property_str(ctx, key).ok()?;
    let s = v.as_string()?;
    Some(s.content().encode_utf8(false).to_string())
}

/// Parse the `env` option: iterate the JS object and collect into a HashMap.
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
