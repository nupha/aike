// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{ExitStatus, Stdio},
};

use zuqe::{
    CallContext, Callback, JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK, JS_PROP_C_W_E,
    JS_PROP_ENUMERABLE, JSResult, JSValue, Object, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use super::{JSStdioHandle, signal::signal_name};

pub(super) fn js_spawn(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_async = cc.magic() == 1;

    // arg 0: command string
    let cmd = cc.arg(0).to_string(ctx)?;
    let cmd_str = cmd.content().encode_utf8(false).to_string();

    // arg 1 (optional): args array
    let mut args: Vec<String> = Vec::new();
    if cc.length() > 1
        && let Some(arr) = cc.arg(1).as_object_Array()
    {
        for v in arr.payload().values() {
            if let Some(s) = v.as_string(true) {
                args.push(s.content().encode_utf8(false).to_string());
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
    if cc.length() > 2
        && let Some(opts) = cc.arg(2).as_object()
    {
        cwd = get_str_prop(ctx, &opts, "cwd");

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
                if let Some(s) = input_val.as_string(true) {
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
    if let Some(data) = input
        && let Some(ref mut stdin) = child.stdin
    {
        let _ = stdin.write_all(&data);
        let _ = stdin.flush();
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
        ctx.create_string_from_str(signal_name(sig as u32).unwrap_or("UNKNOWN"))
            .map(JSValue::make_string)
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
        JSValue::from(exit_code == Some(0) && signal_code.is_none()),
        JS_PROP_C_W_E,
    )?;

    Ok(result.into())
}

fn spawn_async(ctx: &mut ScopedContext, mut child: tokio::process::Child, pid: i32) -> JSResult {
    // Take stdin pipe before moving child into async task,
    // so we can create a PipeWrite FileHandle while ScopedContext is available.
    // When stdin is not piped, this will be None → null.
    let stdin_val = match child.stdin.take() {
        Some(child_stdin) => {
            #[cfg(unix)]
            let fd = child_stdin.as_raw_fd();
            #[cfg(not(unix))]
            let fd = -1;

            match JSStdioHandle::new_instance_pipe_write(ctx, child_stdin, fd) {
                Ok(nob) => JSValue::from(nob),
                Err(_) => JSValue::NULL,
            }
        }
        None => JSValue::NULL,
    };

    // Take stdout pipe before moving child into async task,
    // so we can create a PipeRead StdioHandle while ScopedContext is available.
    // When stdout is not piped, this will be None → null.
    let stdout_val = match child.stdout.take() {
        Some(child_stdout) => {
            #[cfg(unix)]
            let fd = child_stdout.as_raw_fd();
            #[cfg(not(unix))]
            let fd = -1;
            match JSStdioHandle::new_instance_pipe_read(ctx, child_stdout, fd) {
                Ok(nob) => JSValue::from(nob),
                Err(_) => JSValue::NULL,
            }
        }
        None => JSValue::NULL,
    };

    // Take stderr pipe before moving child into async task.
    let stderr_val = match child.stderr.take() {
        Some(child_stderr) => {
            #[cfg(unix)]
            let fd = child_stderr.as_raw_fd();
            #[cfg(not(unix))]
            let fd = -1;
            match JSStdioHandle::new_instance_pipe_read(ctx, child_stderr, fd) {
                Ok(nob) => JSValue::from(nob),
                Err(_) => JSValue::NULL,
            }
        }
        None => JSValue::NULL,
    };

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
                    .map(JSValue::make_string),
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

    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdin", stdin_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", stdout_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", stderr_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", prom_exit.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", prom_signal.into(), JS_PROP_C_W_E)?;

    // kill(signal?) — send signal to child process, default SIGTERM
    let kill_fn = ctx.create_native_function(Callback::Func(js_child_kill), "kill", 1, 0)?;
    result.define_property_value_str(ctx, "kill", kill_fn.into(), JS_PROP_C_W_E)?;

    // isRunning() — check if child process is still alive
    let is_running_fn =
        ctx.create_native_function(Callback::Func(js_child_isRunning), "isRunning", 0, 0)?;
    result.define_property_value_str(ctx, "isRunning", is_running_fn.into(), JS_PROP_C_W_E)?;

    // Spawn a pure async task — wait for child exit and resolve exitCode/signalCode.
    // stdout/stderr are exposed as FileHandle objects for the consumer to read.
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                let _ = tx_exit.send(Ok(code));
                let sig = get_signal_code(&status)
                    .and_then(|n| crate::aike::signal::signal_name(n as u32))
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
#[cfg(unix)]
fn js_child_kill(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::FALSE);
    }

    let sig = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        let arg = cc.arg(0);
        if let Some(s) = arg.as_string(true) {
            let name = s.content().encode_utf8(false);
            crate::aike::signal::signal_number(&name)
                .map(|n| n as i32)
                .unwrap_or(libc::SIGTERM)
        } else {
            arg.to_number(ctx)?.to_i32()
        }
    } else {
        libc::SIGTERM
    };

    let ret = unsafe { libc::kill(pid, sig) };
    Ok(JSValue::from(ret == 0))
}

/// Windows equivalent: kill process by pid (ignores signal arg).
#[cfg(windows)]
fn js_child_kill(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(_ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::FALSE);
    }
    Ok(JSValue::from(unsafe { terminate_process(pid) }))
}
#[cfg(windows)]
unsafe fn terminate_process(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // SAFETY: pid is a valid process id from a spawned child process.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
    if handle.is_null() {
        return false;
    }
    let ok = unsafe { TerminateProcess(handle, 1) != 0 };
    unsafe { CloseHandle(handle) };
    ok
}

/// isRunning() — check if child process is still alive using kill(pid, 0).
#[cfg(unix)]
fn js_child_isRunning(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::FALSE);
    }
    let running = unsafe { libc::kill(pid, 0) == 0 };
    Ok(JSValue::from(running))
}

/// isRunning() — Windows: check if process is alive via WaitForSingleObject.
#[cfg(windows)]
fn js_child_isRunning(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(_ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::FALSE);
    }
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, WaitForSingleObject,
        };

        let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return Ok(JSValue::FALSE);
        }
        let status = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        // WAIT_TIMEOUT = process still running
        Ok(JSValue::from(status == WAIT_TIMEOUT))
    }
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
    let s = v.as_string(true)?;
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
    let Some(mut env_obj) = v.as_object() else {
        return Ok(None);
    };

    let mut map = HashMap::new();
    let pks = env_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    for pk in pks.iter() {
        let key_v = pk.atom.to_value(ctx)?;
        let key_r = key_v.to_string(ctx)?;
        let key = key_r.encode_utf8(false).to_string();

        let val = env_obj.get_property(ctx, pk.atom)?;
        if let Some(s) = val.as_string(true) {
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
