// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

#[cfg(unix)]
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{Command, ExitStatus, Stdio},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap as TokioCmdWrap};
#[cfg(windows)]
use process_wrap::tokio::{JobObject, KillOnDrop};
use zuqe::{
    CallContext, Callback, ErrorKind, Exception, IntOrFloat, JS_GPN_ENUM_ONLY,
    JS_GPN_STR_OR_INT_MASK, JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSResult, JSValue, Object,
    ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use super::{JSStdioHandle, signal::signal_name};

/// PIDs of children spawned with `abortOnParentExit: true`, killed together
/// when aike exits (normal exit, `process.exit`, or a catchable signal).
/// Only populated when the option is set; the default is zero overhead.
static SPAWNED_PIDS: LazyLock<Mutex<Vec<i32>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub(crate) fn register_child(pid: i32) {
    SPAWNED_PIDS.lock().unwrap().push(pid);
}

pub(crate) fn unregister_child(pid: i32) {
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

/// Per-child exit notification state, shared between the exit-wait task and
/// `shutdown()`. The watch channel retains the final value, so a waiter that
/// subscribes before or after the exit never misses the transition.
pub(crate) struct ChildExitState {
    /// true once the child has been reaped (or its wait errored).
    pub(crate) exit_tx: tokio::sync::watch::Sender<bool>,
    /// Guards against concurrent shutdown ladders on one child.
    pub(crate) shutting_down: AtomicBool,
}

pub(crate) static CHILD_EXITS: LazyLock<Mutex<HashMap<i32, ChildExitState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn register_child_exit(pid: i32) {
    let (exit_tx, _) = tokio::sync::watch::channel(false);
    CHILD_EXITS.lock().unwrap().insert(
        pid,
        ChildExitState {
            exit_tx,
            shutting_down: AtomicBool::new(false),
        },
    );
}

/// Signal waiters that `pid` exited and drop its registry entry.
pub(crate) fn signal_child_exit(pid: i32) {
    if let Some(state) = CHILD_EXITS.lock().unwrap().remove(&pid) {
        let _ = state.exit_tx.send(true);
    }
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

    // Whether each stream was configured as "inherit" (defaults included).
    let mut stdin_inherit = true;
    let mut stdout_inherit = false;
    let mut stderr_inherit = false;

    let mut cwd = None;
    let mut env_opt: Option<HashMap<String, String>> = None;
    let mut input_data: Option<Vec<u8>> = None;
    let mut abort_on_parent_exit = false;
    let mut sandbox_opt: Option<super::sandbox::Policy> = None;

    // arg 2 (optional): options object
    if cc.length() > 2 {
        let opts_val = cc.arg(2);
        let Some(opts) = opts_val.as_object() else {
            return Err(ctx.throw_type_error("spawn options must be an object"));
        };

        // cwd: string
        if let Some(dir) = get_str_prop_strict(ctx, &opts, "cwd")? {
            cwd = Some(dir);
        }

        // env: object; replaces entire env when provided
        if let Some(env_map) = parse_env_obj_strict(ctx, &opts)? {
            env_opt = Some(env_map);
        }

        // abortOnParentExit: boolean; when true the child is killed
        // synchronously when aike exits. Meaningless for spawnSync/sh (the
        // child is already gone on return) but still validated.
        let abort_val = opts.get_property_str(ctx, "abortOnParentExit")?;
        if !abort_val.is_undefined() {
            if !abort_val.is_bool() {
                return Err(ctx.throw_type_error("`abortOnParentExit` must be a boolean"));
            }
            abort_on_parent_exit = abort_val.to_bool()?;
            // Process-exit cleanup semantics: root-only.
            if abort_on_parent_exit {
                crate::aike::ensure_root_runtime(ctx, "Aike.spawn({abortOnParentExit})")?;
            }
        }

        // input — extract bytes for spawnSync stdin
        if !is_async {
            input_data = extract_input(ctx, &opts)?;
        }

        // sandbox — optional; when present the command runs inside a sandbox
        // (see super::sandbox) instead of a plain spawn. Validated here;
        // dispatched after timeout parsing below.
        sandbox_opt = super::sandbox::get_sandbox_opt(ctx, &opts)?;

        // stdio modes
        (stdin_mode, stdin_inherit) = parse_stdio_mode_strict(ctx, &opts, "stdin", true)?;
        #[cfg(windows)]
        {
            (stdout_mode, stdout_inherit) = parse_stdio_mode_strict(ctx, &opts, "stdout", false)?;
            (stderr_mode, stderr_inherit) = parse_stdio_mode_strict(ctx, &opts, "stderr", false)?;
        }
        #[cfg(not(windows))]
        {
            stdout_mode = parse_stdio_mode_strict(ctx, &opts, "stdout", false)?.0;
            stderr_mode = parse_stdio_mode_strict(ctx, &opts, "stderr", false)?.0;
        }
    }

    // input implies stdin=pipe
    if input_data.is_some() {
        stdin_mode = Stdio::piped();
        #[cfg(windows)]
        {
            stdin_inherit = false;
        }
    }

    // timeout (ms): positive integer; kill the child if it runs longer than this.
    let timeout_limit: Option<u64> = match cc.arg(2).as_object() {
        Some(opts) => get_timeout_prop(ctx, &opts)?,
        None => None,
    };

    // Sandbox dispatch: sandboxed children always use piped stdio, and the
    // policy's working directory as the cwd. `env` (SpawnOptions.env) is
    // forwarded into the sandbox, so it is NOT ignored.
    if let Some(policy) = sandbox_opt {
        return if is_async {
            super::sandbox::spawn_async_sandboxed(
                ctx,
                &policy,
                cmd_str,
                args,
                env_opt,
                timeout_limit,
                abort_on_parent_exit,
            )
        } else {
            super::sandbox::spawn_sync_sandboxed(
                ctx,
                &policy,
                cmd_str,
                args,
                env_opt,
                input_data,
                timeout_limit,
            )
        };
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

        // Build the command wrapper: ProcessGroup (unix) gives us killpg
        // (whole tree); JobObject (Windows) gives kernel-level kill-on-close.
        // Only wrap when tree-kill is needed (timeout or abortOnParentExit),
        // to avoid changing terminal/foreground semantics for regular spawns.
        let mut cmd_wrap = TokioCmdWrap::from(cmd_obj);
        #[cfg(unix)]
        if timeout_limit.is_some() || abort_on_parent_exit {
            cmd_wrap.wrap(ProcessGroup::leader());
        }
        #[cfg(windows)]
        if timeout_limit.is_some() || abort_on_parent_exit {
            // JobObject alone does NOT set JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // that flag is only enabled when the KillOnDrop wrapper is present
            // too. Without it, exiting aike closes its job handle but leaves
            // abortOnParentExit children running.
            cmd_wrap.wrap(JobObject);
            cmd_wrap.wrap(KillOnDrop);
        }

        // Linux: kernel sends SIGKILL to the child when aike dies; covers
        // SIGKILL/abort/crash that no user-space handler can catch. The child
        // is reaped by `kill(-pid)` too when aike exits normally; PDEATHSIG is
        // the fallback for the uncatchable paths.
        #[cfg(target_os = "linux")]
        if abort_on_parent_exit {
            unsafe {
                cmd_wrap.command_mut().pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                    Ok(())
                });
            }
        }

        let subprocess = {
            // Guard our own stdio against blanket handle inheritance.
            #[cfg(windows)]
            let saved = clear_own_stdio_inherit(stdin_inherit, stdout_inherit, stderr_inherit);
            let res = cmd_wrap.spawn();
            #[cfg(windows)]
            restore_own_stdio_inherit(&saved);
            res.map_err(|e| ctx.throw_internal_error(&format!("spawn failed: {e}")))?
        };

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

        let subprocess = {
            // Guard our own stdio against blanket handle inheritance.
            #[cfg(windows)]
            let saved = clear_own_stdio_inherit(stdin_inherit, stdout_inherit, stderr_inherit);
            let res = cmd_obj.spawn();
            #[cfg(windows)]
            restore_own_stdio_inherit(&saved);
            res.map_err(|e| ctx.throw_internal_error(&format!("spawn failed: {e}")))?
        };

        let pid = subprocess.id() as i32;
        spawn_sync(ctx, subprocess, pid, input_data, timeout_limit)
    }
}

/// `Aike.sh(command, options?)` `{ stdout, stderr, exitCode }`.
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
    let mut sandbox_opt: Option<super::sandbox::Policy> = None;

    // options (optional object): strict parsing, same schema as spawnSync
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
        sandbox_opt = super::sandbox::get_sandbox_opt(ctx, &opts)?;

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

    // A sandboxed `sh` runs through the same backend as `spawnSync`, so the
    // two cannot drift: it used to build its own `Command` here and had no
    // sandbox path at all, which left the option silently ignored on the one
    // entry point an agent reaches for first.
    if let Some(policy) = sandbox_opt {
        let mut argv: Vec<&str> = shell_flags.to_vec();
        argv.push(&command);
        let (exit_code, stdout, stderr, killed) =
            super::sandbox::run_sandboxed_sync(&policy, &shell, &argv, env_opt, input, timeout_ms);
        if check && exit_code != 0 {
            let msg = format!("`command` exited with code {exit_code}");
            return match ctx.create_error_object(ErrorKind::Internal, &msg, false) {
                Some(err) => {
                    ctx.throw(Exception::Error(err.into()));
                    Err(-1)
                }
                None => Err(ctx.throw_internal_error(&msg)),
            };
        }
        return Ok(build_sh_result(ctx, exit_code, stdout, stderr, killed, true)?.into());
    }

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

    let (stdout_buf, stderr_buf, exit_status, killed) =
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

    build_sh_result(
        ctx,
        exit_status.code().unwrap_or(-1),
        stdout_buf,
        stderr_buf,
        killed,
        false,
    )
}

/// The result object `Aike.sh` returns: output as strings, unlike
/// `spawnSync`'s byte arrays. Shared by the plain and sandboxed paths.
fn build_sh_result(
    ctx: &mut ScopedContext,
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    killed_by_timeout: bool,
    sandboxed: bool,
) -> JSResult {
    let stdout_js = JSValue::make_string(
        ctx.create_string_from_utf8(String::from_utf8_lossy(&stdout).as_bytes().to_vec())?,
    );
    let stderr_js = JSValue::make_string(
        ctx.create_string_from_utf8(String::from_utf8_lossy(&stderr).as_bytes().to_vec())?,
    );

    let mut result = ctx.create_base_object()?;
    if sandboxed {
        result.define_property_value_str(ctx, "sandboxed", JSValue::TRUE, JS_PROP_ENUMERABLE)?;
    }
    result.define_property_value_str(ctx, "stdout", stdout_js, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", stderr_js, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", JSValue::from(exit_code), JS_PROP_C_W_E)?;
    result.define_property_value_str(
        ctx,
        "success",
        JSValue::from(exit_code == 0),
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
    // Windows: TerminateProcess only kills the direct child; surviving
    // grandchildren keep the inherited pipe write ends open and block the
    // collector until they exit naturally. Wrap the child in a kill-on-close
    // job so a timeout can take down the whole tree (the async path gets this
    // from process_wrap's JobObject wrapper).
    #[cfg(windows)]
    let job = if timeout_ms.is_some() {
        job_assign_child(&child)
    } else {
        std::ptr::null_mut()
    };

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(collect_child_output(child, input));
    });

    let (collected, killed_by_timeout) = match timeout_ms {
        Some(ms) => match rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(r) => (r, false),
            Err(_) => {
                #[cfg(windows)]
                if !job.is_null() {
                    use windows_sys::Win32::System::JobObjects::TerminateJobObject;
                    unsafe { TerminateJobObject(job, 1) };
                } else {
                    terminate_process_tree(pid);
                }
                #[cfg(unix)]
                terminate_process_tree(pid);

                // Killing closes the child's pipes, so the worker finishes.
                let r = rx.recv().expect("collect thread must finish after kill");
                (r, true)
            }
        },
        None => (rx.recv().expect("collect thread must send"), false),
    };

    // KILL_ON_JOB_CLOSE makes closing safe either way: after natural
    // completion every job member has exited, so nothing gets killed.
    #[cfg(windows)]
    if !job.is_null() {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(job) };
    }

    let (stdout_buf, stderr_buf, exit_status) = collected?;
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
    if let Some(o) = input_val.as_object() {
        if let Some(ta) = o.payload_typed_array() {
            return Ok(Some(ta.bytes().to_vec()));
        } else if let Some(ab) = o.payload_array_buffer() {
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
        ctx.create_string_from_utf8(
            signal_name(sig as u32)
                .unwrap_or("UNKNOWN")
                .as_bytes()
                .to_vec(),
        )
        .map(JSValue::make_string)
    })?;

    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "sandboxed", JSValue::FALSE, JS_PROP_ENUMERABLE)?;
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
    mut child: Box<dyn ChildWrapper>,
    pid: i32,
    timeout_ms: Option<u64>,
    abort_on_parent_exit: bool,
) -> JSResult {
    // Take stdin pipe before moving child into async task,
    // so we can create a PipeWrite FileHandle while ScopedContext is available.
    // When stdin is not piped, this will be None, not null.
    let stdin_val = match child.stdin().take() {
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
    // When stdout is not piped, this will be None, not null.
    let stdout_val = match child.stdout().take() {
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
    let stderr_val = match child.stderr().take() {
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
    let (prom_exit, tx_exit) = Promise::create_with_channel_hold_when_awaited::<Result<i32, String>>(
        ctx,
        |cx, data| -> JSResult {
            match data {
                Ok(code) => Ok(JSValue::from(code)),
                Err(e) => Err(cx.throw_internal_error(&e)),
            }
        },
    )?;

    let (prom_signal, tx_signal) = Promise::create_with_channel_hold_when_awaited::<
        Result<Option<String>, String>,
    >(ctx, |cx, data| -> JSResult {
        match data {
            Ok(Some(name)) => cx
                .create_string_from_utf8(name.into_bytes())
                .map(JSValue::make_string),
            Ok(None) => Ok(JSValue::NULL),
            Err(e) => Err(cx.throw_internal_error(&e)),
        }
    })?;

    let (prom_killed, tx_killed) =
        Promise::create_with_channel_hold_when_awaited::<bool>(ctx, |_, killed| {
            Ok(JSValue::from(killed))
        })?;

    // Build result object with Promises
    let mut result = ctx.create_base_object()?;

    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "sandboxed", JSValue::FALSE, JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdin", stdin_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", stdout_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", stderr_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", prom_exit.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", prom_signal.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "killedByTimeout", prom_killed.into(), JS_PROP_C_W_E)?;

    // kill(signal?) sends a signal to the child process, default SIGTERM
    let kill_fn = ctx.create_native_function(Callback::Func(js_child_kill), "kill", 1, 0)?;
    result.define_property_value_str(ctx, "kill", kill_fn.into(), JS_PROP_C_W_E)?;

    // isRunning() checks if the child process is still alive
    let is_running_fn =
        ctx.create_native_function(Callback::Func(js_child_isRunning), "isRunning", 0, 0)?;
    result.define_property_value_str(ctx, "isRunning", is_running_fn.into(), JS_PROP_C_W_E)?;

    // shutdown(options?) graceful close (EOF, then SIGTERM, then SIGKILL)
    let shutdown_fn =
        ctx.create_native_function(Callback::Func(js_child_shutdown), "shutdown", 1, 0)?;
    result.define_property_value_str(ctx, "shutdown", shutdown_fn.into(), JS_PROP_C_W_E)?;

    // Register the exit notification before spawning the waiter task so
    // shutdown() can subscribe even if the child exits immediately.
    register_child_exit(pid);

    // Spawn a pure async task: wait for child exit and resolve
    // exitCode/signalCode/killedByTimeout. stdout/stderr are exposed as
    // FileHandle objects for the consumer to read.
    if abort_on_parent_exit {
        register_child(pid);
    }

    tokio::spawn(async move {
        let timeout = timeout_ms.map(Duration::from_millis);
        let mut killed = false;
        let result = match timeout {
            Some(d) => match tokio::time::timeout(d, child.wait()).await {
                Ok(r) => r,
                Err(_elapsed) => {
                    killed = true;
                    // Group/job-aware kill: ProcessGroup uses killpg(SIGKILL) to
                    // the whole tree; JobObject uses TerminateJobObject.
                    let _ = Box::into_pin(child.kill()).await;
                    child.wait().await
                }
            },
            None => child.wait().await,
        };
        // The child has exited; it can no longer be killed on aike exit.
        if abort_on_parent_exit {
            unregister_child(pid);
        }
        // Wake shutdown() ladders before resolving the JS promises so they
        // observe a consistent exit state.
        signal_child_exit(pid);
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

/// kill(signal?)
/// - Windows: terminate process by pid (ignores signal arg).
/// - Unix: send signal to child process.
fn js_child_kill(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    #[cfg(unix)]
    {
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

    #[cfg(windows)]
    {
        let pid = pid_from_this(ctx, &cc);
        if pid <= 0 {
            return Ok(JSValue::FALSE);
        }
        Ok(JSValue::from(unsafe { terminate_process(pid) }))
    }
}

/// Terminate process by pid
pub(crate) unsafe fn terminate_process(pid: i32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, libc::SIGKILL) == 0
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_TERMINATE, TerminateProcess,
        };

        // SAFETY: pid is a valid process id from a spawned child process.
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
        if handle.is_null() {
            return false;
        }

        let ok = unsafe { TerminateProcess(handle, 1) != 0 };
        unsafe { CloseHandle(handle) };
        ok
    }
}

/// isRunning()
/// - Unix: check if child process is still alive using kill(pid, 0).
/// - Windows: check if process is alive via WaitForSingleObject.
fn js_child_isRunning(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    #[cfg(unix)]
    {
        let pid = pid_from_this(_ctx, &cc);
        if pid <= 0 {
            return Ok(JSValue::FALSE);
        }
        let running = unsafe { libc::kill(pid, 0) == 0 };
        Ok(JSValue::from(running))
    }
    #[cfg(windows)]
    {
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
}

/// Read `pid` from the `this` object's "pid" property.
pub(crate) fn pid_from_this(ctx: &mut ScopedContext, cc: &CallContext) -> i32 {
    let this = cc.this();
    this.as_object()
        .and_then(|obj| obj.get_property_str(ctx, "pid").ok())
        .and_then(|v| v.to_number(ctx).ok().map(|n| n.to_i32()))
        .unwrap_or(-1)
}

/// Read an optional positive-integer millisecond property.
/// TypeError for non-numbers, RangeError for non-positive or fractional values.
pub(crate) fn get_pos_ms_prop(
    ctx: &mut ScopedContext,
    obj: &Object,
    key: &str,
) -> Result<Option<u64>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`{key}` must be a number")));
    }
    let n = v.to_number(ctx)?;
    match n {
        IntOrFloat::Int64(i) if i > 0 => Ok(Some(i as u64)),
        IntOrFloat::Float64(d) if d > 0.0 && d.fract() == 0.0 => Ok(Some(d as u64)),
        _ => Err(ctx.throw_range_error(&format!("`{key}` must be a positive integer"))),
    }
}

/// Wait until the child's exit is signalled or the timeout elapses.
pub(crate) async fn wait_child_exit(
    mut rx: tokio::sync::watch::Receiver<bool>,
    timeout: Duration,
) -> bool {
    loop {
        if *rx.borrow_and_update() {
            return true;
        }
        match tokio::time::timeout(timeout, rx.changed()).await {
            // changed() errors when the sender is dropped; the state was
            // removed from the registry, which only happens on exit/cleanup.
            Ok(Ok(())) => continue,
            _ => return false,
        }
    }
}

pub(crate) fn send_child_signal(pid: i32, sig: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid, sig);
    }
    #[cfg(windows)]
    {
        let _ = sig;
        unsafe { terminate_process(pid) };
    }
}

/// The EOF, SIGTERM, SIGKILL shutdown ladder. Returns the stage at which the
/// child exited. Removes the registry entry when done.
pub(crate) async fn shutdown_ladder(
    pid: i32,
    rx: tokio::sync::watch::Receiver<bool>,
    eof_ms: u64,
    term_ms: u64,
    kill_ms: u64,
    term_sig: i32,
) -> &'static str {
    macro_rules! finish {
        ($stage:expr) => {{
            signal_child_exit(pid);
            $stage
        }};
    }

    // 1) EOF: stdin was closed; give the child time to exit on its own.
    if wait_child_exit(rx.clone(), Duration::from_millis(eof_ms)).await {
        return finish!("eof");
    }

    // 2) termSignal: gentle request.
    send_child_signal(pid, term_sig);
    if wait_child_exit(rx.clone(), Duration::from_millis(term_ms)).await {
        return finish!("term");
    }

    // 3) SIGKILL: forced.
    #[cfg(unix)]
    send_child_signal(pid, libc::SIGKILL);
    #[cfg(windows)]
    send_child_signal(pid, 0);
    if wait_child_exit(rx, Duration::from_millis(kill_ms)).await {
        return finish!("kill");
    }

    finish!("timed_out")
}

/// shutdown(options?) gracefully shuts the child down:
/// close stdin (EOF), then `termSignal`, then SIGKILL; each stage bounded by
/// its timeout. Resolves with the stage at which the child exited:
/// "eof" | "term" | "kill" | "timed_out" | "already_closed".
fn js_child_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    /// Resolve a promise to a fixed stage string.
    fn resolved_stage(ctx: &mut ScopedContext, stage: &'static str) -> JSResult {
        let (promise, tx) = Promise::create_with_channel::<String>(ctx, |cx, stage| {
            cx.create_string_from_utf8(stage.as_bytes().to_vec())
                .map(JSValue::make_string)
        })?;
        let _ = tx.send(stage.to_string());
        Ok(promise.into())
    }

    // Strict option parsing (defaults mirror ravenclaw's StdioParentOptions).
    let mut eof_ms = 2_000u64;
    let mut term_ms = 2_000u64;
    let mut kill_ms = 1_000u64;
    let mut term_sig = libc::SIGTERM;
    if cc.length() > 0 && !cc.arg(0).is_undefined() {
        let Some(opts) = cc.arg(0).as_object() else {
            return Err(ctx.throw_type_error("shutdown options must be an object"));
        };
        eof_ms = get_pos_ms_prop(ctx, &opts, "eofTimeoutMs")?.unwrap_or(eof_ms);
        term_ms = get_pos_ms_prop(ctx, &opts, "termTimeoutMs")?.unwrap_or(term_ms);
        kill_ms = get_pos_ms_prop(ctx, &opts, "killTimeoutMs")?.unwrap_or(kill_ms);

        let sig_val = opts.get_property_str(ctx, "termSignal")?;
        if !sig_val.is_undefined() {
            let Some(s) = sig_val.as_string(true) else {
                return Err(ctx.throw_type_error("`termSignal` must be a string"));
            };
            let name = s.content().encode_utf8(false);
            match name.as_ref() {
                "SIGTERM" | "SIGINT" => {
                    term_sig = crate::aike::signal::signal_number(name.as_ref())
                        .map(|n| n as i32)
                        .unwrap_or(libc::SIGTERM)
                }
                _ => {
                    return Err(
                        ctx.throw_range_error("`termSignal` must be \"SIGTERM\" or \"SIGINT\"")
                    );
                }
            }
        }
    }

    let pid = pid_from_this(ctx, &cc);

    // Claim the child's exit state: absent, never spawned by us or already
    // reaped; already shutting down. Report without re-running the ladder.
    let rx = {
        let mut map = CHILD_EXITS.lock().unwrap();
        let Some(state) = map.get_mut(&pid) else {
            drop(map);
            return resolved_stage(ctx, "already_closed");
        };
        if state.shutting_down.swap(true, Ordering::AcqRel) {
            drop(map);
            return resolved_stage(ctx, "already_closed");
        }
        state.exit_tx.subscribe()
    };

    // Close stdin so the child sees EOF (drop closes the pipe fd).
    {
        let this = cc.this();
        if let Some(obj) = this.as_object()
            && let Ok(stdin_val) = obj.get_property_str(ctx, "stdin")
        {
            let mut v = stdin_val;
            JSStdioHandle::take_payload_of(&mut v);
        }
    }

    let (promise, tx) = Promise::create_with_channel::<String>(ctx, |cx, stage| {
        cx.create_string_from_utf8(stage.as_bytes().to_vec())
            .map(JSValue::make_string)
    })?;

    tokio::spawn(async move {
        let stage = shutdown_ladder(pid, rx, eof_ms, term_ms, kill_ms, term_sig).await;
        let _ = tx.send_async(stage.to_string()).await;
    });

    Ok(promise.into())
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
/// Returns the mode plus whether the effective mode is "inherit" (used by
/// the Windows spawn guard to know which of our own handles must stay
/// inheritable).
fn parse_stdio_mode_strict(
    ctx: &mut ScopedContext,
    opts: &Object,
    key: &str,
    default_inherit: bool,
) -> Result<(Stdio, bool), i32> {
    let Some(mode) = get_str_prop_strict(ctx, opts, key)? else {
        let stdio = if default_inherit {
            Stdio::inherit()
        } else {
            Stdio::piped()
        };
        return Ok((stdio, default_inherit));
    };
    match mode.as_str() {
        "pipe" => Ok((Stdio::piped(), false)),
        "inherit" => Ok((Stdio::inherit(), true)),
        "ignore" => Ok((Stdio::null(), false)),
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

/// Windows: create a kill-on-close job object and assign a sync-spawned child
/// to it, so killing the job takes down descendant processes too. Returns a
/// null handle on failure (caller falls back to killing only the direct child).
#[cfg(windows)]
fn job_assign_child(child: &std::process::Child) -> windows_sys::Win32::Foundation::HANDLE {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: handles are freshly created or belong to a live spawned child;
    // each failure path closes the job and degrades to the null handle.
    unsafe {
        let job: HANDLE = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return std::ptr::null_mut();
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) != 0
            && AssignProcessToJobObject(job, child.as_raw_handle()) != 0;
        if !ok {
            CloseHandle(job);
            return std::ptr::null_mut();
        }
        job
    }
}

/// Windows std spawns cannot limit handle inheritance on stable Rust (no
/// stable PROC_THREAD_ATTRIBUTE_HANDLE_LIST), so CreateProcess duplicates
/// every inheritable handle into the child, including our own stdio. A
/// detached grandchild then keeps our stdout/stderr pipes open forever,
/// hanging read-to-EOF collectors (spawnSync/sh). Temporarily clearing the
/// inherit bit on the streams a spawn does NOT pass as "inherit" closes that
/// leak.
///
/// ponytail: flip/restore races with concurrent spawns from other threads;
/// acceptable because aike spawns from its single JS thread.
#[cfg(windows)]
fn clear_own_stdio_inherit(
    keep_stdin: bool,
    keep_stdout: bool,
    keep_stderr: bool,
) -> [Option<u32>; 3] {
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    [
        (!keep_stdin)
            .then(|| std_handle_set_inherit(STD_INPUT_HANDLE, false))
            .flatten(),
        (!keep_stdout)
            .then(|| std_handle_set_inherit(STD_OUTPUT_HANDLE, false))
            .flatten(),
        (!keep_stderr)
            .then(|| std_handle_set_inherit(STD_ERROR_HANDLE, false))
            .flatten(),
    ]
}

/// Re-apply the flags saved by `clear_own_stdio_inherit`.
#[cfg(windows)]
fn restore_own_stdio_inherit(saved: &[Option<u32>; 3]) {
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    const WHICH: [windows_sys::Win32::System::Console::STD_HANDLE; 3] =
        [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    for (which, flags) in WHICH.iter().zip(saved.iter()) {
        if let Some(flags) = flags {
            std_handle_set_flags(*which, *flags);
        }
    }
}

/// Set (inherit=true) or clear (inherit=false) HANDLE_FLAG_INHERIT on one
/// standard handle. Returns the full previous flag word, or None when there
/// is no such handle or the call failed.
#[cfg(windows)]
fn std_handle_set_inherit(
    which: windows_sys::Win32::System::Console::STD_HANDLE,
    inherit: bool,
) -> Option<u32> {
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;

    let old = std_handle_get_flags(which)?;
    let new = if inherit {
        old | HANDLE_FLAG_INHERIT
    } else {
        old & !HANDLE_FLAG_INHERIT
    };
    std_handle_set_flags(which, new).then_some(old)
}

#[cfg(windows)]
fn std_handle_get_flags(which: windows_sys::Win32::System::Console::STD_HANDLE) -> Option<u32> {
    use windows_sys::Win32::Foundation::{GetHandleInformation, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::GetStdHandle;

    unsafe {
        // SAFETY: plain handle queries; failures degrade to None.
        let h = GetStdHandle(which);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut flags: u32 = 0;
        if GetHandleInformation(h, &mut flags) == 0 {
            return None;
        }
        Some(flags)
    }
}

#[cfg(windows)]
fn std_handle_set_flags(
    which: windows_sys::Win32::System::Console::STD_HANDLE,
    flags: u32,
) -> bool {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::GetStdHandle;

    unsafe {
        // SAFETY: plain handle update; failure is reported to the caller.
        let h = GetStdHandle(which);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return false;
        }
        // Only touch the INHERIT bit, never other flag bits (e.g. protect-
        // from-close) that may be present in `flags`.
        SetHandleInformation(h, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT) != 0
    }
}

/// Kill a single child process. Used by `kill_all_children` (atexit /
/// signal handler backstop) as a fallback when process-wrap handles are
/// not available. On unix the child is typically a process-group leader,
/// so `kill(-pid)` reaches all descendants.
fn terminate_process_tree(pid: i32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    #[cfg(windows)]
    unsafe {
        terminate_process(pid)
    };
}

/// Get signal number (Unix-only).
fn get_signal_code(_status: &ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        _status.signal()
    }
    #[cfg(not(unix))]
    {
        None
    }
}
