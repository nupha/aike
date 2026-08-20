// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{Command, ExitStatus, Stdio},
    sync::{LazyLock, Mutex, mpsc},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::process::CommandExt};

use zuqe::{
    CallContext, Callback, ErrorKind, Exception, IntOrFloat, JS_GPN_ENUM_ONLY,
    JS_GPN_STR_OR_INT_MASK, JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSResult, JSValue, Object,
    ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use super::{JSStdioHandle, signal::signal_name};

/// PIDs of children spawned with `abortOnParentExit: true`, killed together
/// when aike exits (normal exit, `process.exit`, or a catchable signal).
/// Only populated when the option is set — the default is zero overhead.
static SPAWNED_PIDS: LazyLock<Mutex<Vec<i32>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn register_child(pid: i32) {
    SPAWNED_PIDS.lock().unwrap().push(pid);
}

fn unregister_child(pid: i32) {
    SPAWNED_PIDS.lock().unwrap().retain(|&p| p != pid);
}

/// Kill all registered child process trees. Called from `atexit` and the
/// default signal handlers; uses `try_lock` so a signal arriving while the
/// registry is briefly held is skipped rather than deadlocking (best-effort
/// in that rare race).
pub(crate) fn kill_all_children() {
    let pids: Vec<i32> = match SPAWNED_PIDS.try_lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
    for pid in pids {
        terminate_process_tree(pid);
    }
    SPAWNED_PIDS.lock().unwrap().clear();
}

pub(super) fn js_spawn(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let is_async = cc.magic() == 1;

    // arg 0: command string
    let cmd_str = match cc.arg(0).as_string(true) {
        Some(s) => s.content().encode_utf8(false).to_string(),
        None => return Err(ctx.throw_type_error("spawn command must be a string")),
    };

    // arg 1 (optional): args array of strings
    let mut args: Vec<String> = Vec::new();
    if cc.length() > 1 && !cc.arg(1).is_undefined() {
        let Some(arr) = cc.arg(1).as_object_Array() else {
            return Err(ctx.throw_type_error("spawn args must be an array"));
        };
        for v in arr.payload().values() {
            let Some(s) = v.as_string(true) else {
                return Err(ctx.throw_type_error("spawn args must be an array of strings"));
            };
            args.push(s.content().encode_utf8(false).to_string());
        }
    }

    // Defaults: inherit stdin, pipe stdout/stderr
    let mut stdin_mode = Stdio::inherit();
    let mut stdout_mode = Stdio::piped();
    let mut stderr_mode = Stdio::piped();

    let mut cwd = None;
    let mut env_opt: Option<HashMap<String, String>> = None;
    let mut input_data: Option<Vec<u8>> = None;
    let mut abort_on_parent_exit = false;

    // arg 2 (optional): options object
    if cc.length() > 2 {
        let opts_val = cc.arg(2);
        let Some(opts) = opts_val.as_object() else {
            return Err(ctx.throw_type_error("spawn options must be an object"));
        };

        // cwd — string
        if let Some(dir) = get_str_prop_strict(ctx, &opts, "cwd")? {
            cwd = Some(dir);
        }

        // env — object; replaces entire env when provided
        if let Some(env_map) = parse_env_obj_strict(ctx, &opts)? {
            env_opt = Some(env_map);
        }

        // abortOnParentExit — boolean; when true the child is killed
        // synchronously when aike exits. Meaningless for spawnSync/sh (the
        // child is already gone on return) but still validated.
        let abort_val = opts.get_property_str(ctx, "abortOnParentExit")?;
        if !abort_val.is_undefined() {
            if !abort_val.is_bool() {
                return Err(ctx.throw_type_error("`abortOnParentExit` must be a boolean"));
            }
            abort_on_parent_exit = abort_val.to_bool()?;
        }

        // input — extract bytes for spawnSync stdin
        if !is_async {
            input_data = extract_input(ctx, &opts)?;
        }

        // stdio modes
        stdin_mode = parse_stdio_mode_strict(ctx, &opts, "stdin", stdin_mode)?;
        stdout_mode = parse_stdio_mode_strict(ctx, &opts, "stdout", stdout_mode)?;
        stderr_mode = parse_stdio_mode_strict(ctx, &opts, "stderr", stderr_mode)?;
    }

    // input implies stdin=pipe
    if input_data.is_some() {
        stdin_mode = Stdio::piped();
    }

    // timeout (ms) — positive integer; kill the child if it runs longer than this.
    let timeout_limit: Option<u64> = match cc.arg(2).as_object() {
        Some(opts) => get_timeout_prop(ctx, &opts)?,
        None => None,
    };

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

        // Run the child in its own process group so a timeout or
        // abort-on-parent-exit can kill the whole tree. Only when requested,
        // to avoid changing terminal/foreground semantics for regular spawns.
        if timeout_limit.is_some() || abort_on_parent_exit {
            #[cfg(unix)]
            cmd_obj.process_group(0);
        }

        // Linux: kernel sends SIGKILL to the child when aike dies — covers
        // SIGKILL/abort/crash that no user-space handler can catch. The child
        // is reaped by `kill(-pid)` too when aike exits normally; PDEATHSIG is
        // the fallback for the uncatchable paths.
        #[cfg(target_os = "linux")]
        if abort_on_parent_exit {
            unsafe {
                cmd_obj.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                    Ok(())
                });
            }
        }

        let subprocess = cmd_obj
            .spawn()
            .map_err(|e| ctx.throw_internal_error(&format!("spawn failed: {e}")))?;

        let pid = subprocess.id().map(|id| id as i32).unwrap_or(-1);
        spawn_async(ctx, subprocess, pid, timeout_limit, abort_on_parent_exit)
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

        if timeout_limit.is_some() {
            #[cfg(unix)]
            cmd_obj.process_group(0);
        }

        let subprocess = cmd_obj
            .spawn()
            .map_err(|e| ctx.throw_internal_error(&format!("spawn failed: {e}")))?;

        let pid = subprocess.id() as i32;
        spawn_sync(ctx, subprocess, pid, input_data, timeout_limit)
    }
}

/// `Aike.sh(command, options?)` → `{ stdout, stderr, exitCode }`.
///
/// Run a command through the platform shell and capture its output. A
/// convenience sugar over `spawnSync` (memo §5.4): the command string is
/// interpreted by the platform shell, so pipelines/redirects/env expansion
/// work. POSIX uses `/bin/sh -c` (guaranteed by POSIX); Windows uses
/// `%COMSPEC%` (default `cmd.exe`) with `/d /s /c`, matching Node. stdout/stderr
/// are always captured and returned as strings.
pub(super) fn js_sh(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let command = cc
        .arg(0)
        .as_string(false)
        .ok_or_else(|| ctx.throw_type_error("`command` must be a string"))?
        .content()
        .encode_utf8(false)
        .to_string();

    let mut cwd = None;
    let mut env_opt = None;
    let mut input = None;
    let mut timeout_ms = None;
    let mut check = false;

    // options (optional object) — strict parsing, same schema as spawnSync
    if cc.length() > 1 {
        let opts_val = cc.arg(1);
        let Some(opts) = opts_val.as_object() else {
            return Err(ctx.throw_type_error("sh options must be an object"));
        };

        if let Some(dir) = get_str_prop_strict(ctx, &opts, "cwd")? {
            cwd = Some(dir);
        }
        if let Some(env_map) = parse_env_obj_strict(ctx, &opts)? {
            env_opt = Some(env_map);
        }
        input = extract_input(ctx, &opts)?;
        timeout_ms = get_timeout_prop(ctx, &opts)?;

        let check_val = opts.get_property_str(ctx, "check")?;
        if !check_val.is_undefined() {
            if !check_val.is_bool() {
                return Err(ctx.throw_type_error("`check` must be a boolean"));
            }
            check = check_val.to_bool()?;
        }
    }

    // Platform shell: POSIX `/bin/sh -c` (guaranteed by POSIX); Windows uses
    // %COMSPEC% (`cmd.exe`) with /d (no AutoRun) and /s (normalized quote
    // stripping), mirroring Node's exec.
    let (shell, shell_flags): (String, &[&str]) = if cfg!(windows) {
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
            &["/d", "/s", "/c"],
        )
    } else {
        ("/bin/sh".to_string(), &["-c"])
    };

    let mut cmd = Command::new(&shell);
    cmd.args(shell_flags).arg(&command);
    // Never inherit stdin: feed input when given, otherwise close it so a child
    // reading stdin sees EOF instead of hanging on the terminal.
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Some(env_map) = env_opt {
        cmd.env_clear();
        cmd.envs(env_map);
    }
    // Own process group so a timeout can kill the whole tree.
    if timeout_ms.is_some() {
        #[cfg(unix)]
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .map_err(|e| ctx.throw_internal_error(&format!("spawn failed: {e}")))?;
    let pid = child.id() as i32;

    let (stdout_buf, stderr_buf, exit_status, _killed) =
        match collect_with_timeout(child, pid, input, timeout_ms) {
            Ok(t) => t,
            Err(e) => return Err(ctx.throw_internal_error(&e)),
        };

    // `check`: throw when the command didn't succeed (non-zero or signal).
    let success = exit_status.code() == Some(0) && get_signal_code(&exit_status).is_none();
    if check && !success {
        let msg = match exit_status.code() {
            Some(code) => format!("`command` exited with code {code}"),
            None => "`command` was terminated by a signal".to_string(),
        };
        return match ctx.create_error_object(ErrorKind::Internal, &msg, false) {
            Some(err) => {
                ctx.throw(Exception::Error(err.into()));
                Err(-1)
            }
            None => Err(ctx.throw_internal_error(&msg)),
        };
    }

    let stdout_js =
        JSValue::make_string(ctx.create_string_from_str(&String::from_utf8_lossy(&stdout_buf))?);
    let stderr_js =
        JSValue::make_string(ctx.create_string_from_str(&String::from_utf8_lossy(&stderr_buf))?);

    let mut result = ctx.create_base_object()?;
    result.define_property_value_str(ctx, "stdout", stdout_js, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", stderr_js, JS_PROP_C_W_E)?;
    result.define_property_value_str(
        ctx,
        "exitCode",
        exit_status.code().map_or(JSValue::NULL, JSValue::from),
        JS_PROP_C_W_E,
    )?;

    Ok(result.into())
}

fn spawn_sync(
    ctx: &mut ScopedContext,
    child: std::process::Child,
    pid: i32,
    input: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
) -> JSResult {
    let (stdout_buf, stderr_buf, exit_status, killed_by_timeout) =
        match collect_with_timeout(child, pid, input, timeout_ms) {
            Ok(t) => t,
            Err(e) => return Err(ctx.throw_internal_error(&e)),
        };
    build_sync_result(
        ctx,
        pid,
        stdout_buf,
        stderr_buf,
        exit_status,
        killed_by_timeout,
    )
}

/// Collect a child's output with an optional timeout, killing the process tree
/// when it overruns. Shared by `spawnSync` and `Aike.sh`.
///
/// The blocking reads/wait run on a worker thread so the caller can apply a
/// timeout: recv_timeout returns, then we kill the tree and drain the
/// (now-finished) worker result.
fn collect_with_timeout(
    child: std::process::Child,
    pid: i32,
    input: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
) -> Result<(Vec<u8>, Vec<u8>, ExitStatus, bool), String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(collect_child_output(child, input));
    });

    let (collected, killed_by_timeout) = match timeout_ms {
        Some(ms) => match rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(r) => (r, false),
            Err(_) => {
                terminate_process_tree(pid);
                // Killing closes the child's pipes, so the worker finishes.
                let r = rx.recv().expect("collect thread must finish after kill");
                (r, true)
            }
        },
        None => (rx.recv().expect("collect thread must send"), false),
    };

    let (stdout_buf, stderr_buf, exit_status) = match collected {
        Ok(t) => t,
        Err(e) => return Err(e),
    };
    Ok((stdout_buf, stderr_buf, exit_status, killed_by_timeout))
}

/// Write input, read stdout/stderr to end, and wait for the child to exit.
/// Runs on a worker thread so the caller can time it out.
fn collect_child_output(
    mut child: std::process::Child,
    input: Option<Vec<u8>>,
) -> Result<(Vec<u8>, Vec<u8>, ExitStatus), String> {
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

    let exit_status = child.wait().map_err(|e| format!("wait failed: {e}"))?;
    Ok((stdout_buf, stderr_buf, exit_status))
}

/// Extract the `input` option (string / TypedArray / ArrayBuffer) as bytes to
/// write to the child's stdin. Shared by `spawnSync` and `Aike.sh`.
fn extract_input(ctx: &mut ScopedContext, opts: &Object) -> Result<Option<Vec<u8>>, i32> {
    let input_val = opts
        .get_property_str(ctx, "input")
        .unwrap_or(JSValue::UNDEFINED);

    if input_val.is_undefined() {
        return Ok(None);
    }
    if let Some(s) = input_val.as_string(true) {
        return Ok(Some(
            s.content().encode_utf8(false).into_owned().into_bytes(),
        ));
    }
    if let Some(payload) = input_val.payload() {
        if let Some(ta) = payload.typed_array() {
            return Ok(Some(ta.bytes().to_vec()));
        }
        if let Some(ab) = payload.array_buffer() {
            return Ok(Some(ab.bytes().to_vec()));
        }
    }
    Err(ctx.throw_type_error("`input` must be a string, TypedArray, or ArrayBuffer"))
}

/// Build the SyncSubprocess object from collected child output.
fn build_sync_result(
    ctx: &mut ScopedContext,
    pid: i32,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    exit_status: ExitStatus,
    killed_by_timeout: bool,
) -> JSResult {
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
    result.define_property_value_str(
        ctx,
        "killedByTimeout",
        JSValue::from(killed_by_timeout),
        JS_PROP_C_W_E,
    )?;

    Ok(result.into())
}

fn spawn_async(
    ctx: &mut ScopedContext,
    mut child: tokio::process::Child,
    pid: i32,
    timeout_ms: Option<u64>,
    abort_on_parent_exit: bool,
) -> JSResult {
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

    // The three result promises keep the event loop alive only while they are
    // observed (awaited / `.then`-ed), so an unawaited (fire-and-forget) child
    // does not block aike from exiting; an awaited child still does.
    let (prom_exit, tx_exit) =
        Promise::create_with_channel_hold_when_awaited::<Result<i32, String>>(
            ctx,
            |cx, data| -> JSResult {
                match data {
                    Ok(code) => Ok(JSValue::from(code)),
                    Err(e) => Err(cx.throw_internal_error(&e)),
                }
            },
        )?;

    let (prom_signal, tx_signal) =
        Promise::create_with_channel_hold_when_awaited::<Result<Option<String>, String>>(
            ctx,
            |cx, data| -> JSResult {
                match data {
                    Ok(Some(name)) => cx
                        .create_string_from_utf8(name.into_bytes())
                        .map(JSValue::make_string),
                    Ok(None) => Ok(JSValue::NULL),
                    Err(e) => Err(cx.throw_internal_error(&e)),
                }
            },
        )?;

    let (prom_killed, tx_killed) =
        Promise::create_with_channel_hold_when_awaited::<bool>(ctx, |cx, killed| {
            Ok(JSValue::from(killed))
        })?;

    // Build result object with Promises
    let mut result = ctx.create_base_object()?;

    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdin", stdin_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", stdout_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", stderr_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", prom_exit.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", prom_signal.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "killedByTimeout", prom_killed.into(), JS_PROP_C_W_E)?;

    // kill(signal?) — send signal to child process, default SIGTERM
    let kill_fn = ctx.create_native_function(Callback::Func(js_child_kill), "kill", 1, 0)?;
    result.define_property_value_str(ctx, "kill", kill_fn.into(), JS_PROP_C_W_E)?;

    // isRunning() — check if child process is still alive
    let is_running_fn =
        ctx.create_native_function(Callback::Func(js_child_isRunning), "isRunning", 0, 0)?;
    result.define_property_value_str(ctx, "isRunning", is_running_fn.into(), JS_PROP_C_W_E)?;

    // Spawn a pure async task — wait for child exit and resolve
    // exitCode/signalCode/killedByTimeout. stdout/stderr are exposed as
    // FileHandle objects for the consumer to read.
    if abort_on_parent_exit {
        register_child(pid);
        // Windows: put the child in a kill-on-close job so it dies with aike
        // even on uncatchable termination (crash, TerminateProcess).
        #[cfg(windows)]
        win_job::assign_abort_on_exit(pid);
    }

    tokio::spawn(async move {
        let timeout = timeout_ms.map(Duration::from_millis);
        let mut killed = false;
        let result = match timeout {
            Some(d) => match tokio::time::timeout(d, child.wait()).await {
                Ok(r) => r,
                Err(_elapsed) => {
                    killed = true;
                    let pid = child.id().map(|id| id as i32).unwrap_or(-1);
                    // Kill the whole tree; taskkill on Windows blocks, so run
                    // it on a blocking thread.
                    let _ = tokio::task::spawn_blocking(move || terminate_process_tree(pid)).await;
                    // Reap the (now-killed) child; on Windows this is also the kill.
                    let _ = child.kill().await;
                    child.wait().await
                }
            },
            None => child.wait().await,
        };
        // The child has exited; it can no longer be killed on aike exit.
        if abort_on_parent_exit {
            unregister_child(pid);
            #[cfg(windows)]
            win_job::release(pid);
        }
        match result {
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                let _ = tx_exit.send(Ok(code));
                let sig = get_signal_code(&status)
                    .and_then(|n| super::signal::signal_name(n as u32))
                    .map(String::from);
                let _ = tx_signal.send(Ok(sig));
                let _ = tx_killed.send(killed);
            }
            Err(e) => {
                let msg = e.to_string();
                let _ = tx_exit.send(Err(msg.clone()));
                let _ = tx_signal.send(Err(msg));
                let _ = tx_killed.send(killed);
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
            super::signal::signal_number(&name)
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

/// kill(signal?) — Windows: terminate process by pid (ignores signal arg).
#[cfg(windows)]
fn js_child_kill(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(_ctx, &cc);
    if pid <= 0 {
        return Ok(JSValue::FALSE);
    }
    Ok(JSValue::from(unsafe { terminate_process(pid) }))
}

/// Force-kill process by pid (SIGKILL). Unix equivalent of Windows TerminateProcess.
#[cfg(unix)]
pub(crate) unsafe fn terminate_process(pid: i32) -> bool {
    unsafe { libc::kill(pid, libc::SIGKILL) == 0 }
}
/// Windows: terminate process by pid (ignores signal arg).
#[cfg(windows)]
pub(crate) unsafe fn terminate_process(pid: i32) -> bool {
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

/// Read a string property strictly; Ok(None) when absent, TypeError when the
/// value is present but not a string.
fn get_str_prop_strict(
    ctx: &mut ScopedContext,
    obj: &Object,
    key: &str,
) -> Result<Option<String>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    match v.as_string(true) {
        Some(s) => Ok(Some(s.content().encode_utf8(false).to_string())),
        None => Err(ctx.throw_type_error(&format!("`{key}` must be a string"))),
    }
}

/// Read the `timeout` option: a positive integer number of milliseconds.
/// TypeError for non-numbers, RangeError for non-positive or fractional values.
fn get_timeout_prop(ctx: &mut ScopedContext, obj: &Object) -> Result<Option<u64>, i32> {
    let v = obj.get_property_str(ctx, "timeout")?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error("`timeout` must be a number"));
    }
    let n = v.to_number(ctx)?;
    match n {
        IntOrFloat::Int64(i) if i > 0 => Ok(Some(i as u64)),
        IntOrFloat::Float64(d) if d > 0.0 && d.fract() == 0.0 => Ok(Some(d as u64)),
        _ => Err(ctx.throw_range_error("`timeout` must be a positive integer")),
    }
}

/// Parse a stdio mode property strictly: "pipe" | "inherit" | "ignore".
fn parse_stdio_mode_strict(
    ctx: &mut ScopedContext,
    opts: &Object,
    key: &str,
    default: Stdio,
) -> Result<Stdio, i32> {
    let Some(mode) = get_str_prop_strict(ctx, opts, key)? else {
        return Ok(default);
    };
    match mode.as_str() {
        "pipe" => Ok(Stdio::piped()),
        "inherit" => Ok(Stdio::inherit()),
        "ignore" => Ok(Stdio::null()),
        _ => Err(ctx.throw_range_error(&format!(
            "`{key}` must be \"pipe\", \"inherit\", or \"ignore\""
        ))),
    }
}

/// Parse the `env` option strictly: an object mapping string keys to string
/// values; replaces the entire env when provided.
fn parse_env_obj_strict(
    ctx: &mut ScopedContext,
    opts: &Object,
) -> Result<Option<HashMap<String, String>>, i32> {
    let v = opts.get_property_str(ctx, "env")?;
    if v.is_undefined() {
        return Ok(None);
    }
    let Some(mut env_obj) = v.as_object() else {
        return Err(ctx.throw_type_error("`env` must be an object"));
    };

    let mut map = HashMap::new();
    let pks = env_obj.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
    for pk in pks.iter() {
        let key_v = pk.atom.to_value(ctx)?;
        let key_r = key_v.to_string(ctx)?;
        let key = key_r.encode_utf8(false).to_string();

        let val = env_obj.get_property(ctx, pk.atom)?;
        let Some(s) = val.as_string(true) else {
            return Err(ctx.throw_type_error("`env` values must be strings"));
        };
        map.insert(key, s.content().encode_utf8(false).to_string());
    }
    Ok(Some(map))
}

/// Kill the child and its whole process tree on timeout.
/// Unix: the child was made a process-group leader via `process_group(0)`, so
/// `kill(-pid)` takes down all descendants. Windows: `taskkill /T /F` walks
/// the tree; falls back to TerminateProcess (direct process only).
#[cfg(unix)]
fn terminate_process_tree(pid: i32) {
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}
#[cfg(windows)]
fn terminate_process_tree(pid: i32) {
    use std::os::windows::process::CommandExt;
    // Hide the taskkill console window when aike runs without a console.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let ok = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        unsafe { terminate_process(pid) };
    }
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

/// Windows: assign a child to a kill-on-close job object. When aike dies —
/// normally, via crash, or via TerminateProcess — the OS closes every handle
/// and kills the child (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`). This covers
/// paths no user-space handler can reach (the Windows analogue of Linux
/// `PR_SET_PDEATHSIG`). Best-effort: failure falls back to the console-ctrl /
/// atexit kill via the PID registry.
#[cfg(windows)]
pub(crate) mod win_job {
    use std::sync::{LazyLock, Mutex};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    };

    static JOBS: LazyLock<Mutex<Vec<(i32, HANDLE)>>> =
        LazyLock::new(|| Mutex::new(Vec::new()));

    /// Put `pid` in a kill-on-close job object. No-op on failure.
    pub(crate) fn assign_abort_on_exit(pid: i32) {
        unsafe {
            // Anonymous job object (no name).
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                CloseHandle(job);
                return;
            }
            let handle = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid as u32);
            if handle.is_null() {
                CloseHandle(job);
                return;
            }
            let ok = AssignProcessToJobObject(job, handle);
            CloseHandle(handle);
            if ok == 0 {
                CloseHandle(job);
                return;
            }
            // Keep the job handle alive until process exit (or child exit).
            JOBS.lock().unwrap().push((pid, job));
        }
    }

    /// Drop the job for a child that already exited — no longer needs
    /// kill-on-close, and avoids leaking job handles on long-running hosts.
    pub(crate) fn release(pid: i32) {
        let mut jobs = JOBS.lock().unwrap();
        if let Some(idx) = jobs.iter().position(|&(p, _)| p == pid) {
            unsafe { CloseHandle(jobs.remove(idx).1) };
        }
    }
}

/// Windows: console control handler (Ctrl-C / Ctrl-Break) — kill
/// `abortOnParentExit` children synchronously, then let default handling
/// terminate aike (which also closes the kill-on-close job handles).
#[cfg(windows)]
pub(crate) fn install_console_ctrl_handler() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        kill_all_children();
        // FALSE: continue to the next handler / default termination.
        0
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}
