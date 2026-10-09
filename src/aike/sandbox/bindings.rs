// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2026 John Ray <996351336@qq.com>

//! The sandbox module's JS interface: `SandboxOptions` in, `Subprocess` out.
//!
//! Three groups, all of them the boundary between the engine and the backends:
//!
//! - option parsing — `get_sandbox_opt` turns the `sandbox` object into a
//!   validated [`Policy`], rejecting anything the backends cannot honour;
//! - the spawn paths — `spawn_async_sandboxed`, `spawn_sync_sandboxed` and
//!   `run_sandboxed_sync`, which pick up a [`SpawnedChild`] and hand its pipes
//!   to the engine;
//! - the per-child methods — `kill` / `isRunning` / `shutdown` on the object
//!   `spawn` returns, plus `Aike.sandboxSupport()`.
//!
//! Everything here is backend-agnostic. Which OS primitive enforces the policy
//! is decided once, in [`super::spawn`]; this module only moves bytes between
//! the engine and pipes.

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// The raw descriptor a pipe is exposed to the engine under. Windows has no
/// `RawFd`; the handle value is not used there and `-1` matches what the plain
/// spawn path passes.
fn raw_fd(file: &std::fs::File) -> i32 {
    #[cfg(unix)]
    {
        file.as_raw_fd()
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        -1
    }
}

use zuqe::{
    Array, CallContext, Callback, JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSResult, JSValue, Object,
    ScopedContext,
    builtins::{
        promise::{Promise, PromiseResultSlot},
        typed_array::TypedArray,
    },
};

use crate::aike::{
    JSStdioHandle, pipe_bridge,
    signal::signal_name,
    spawn::{
        get_pos_ms_prop, pid_from_this, register_child, register_child_exit, shutdown_ladder,
        signal_child_exit, unregister_child,
    },
};

use super::{
    child::SpawnedChild,
    policy::{NetworkPolicy, Policy},
};

// ---------------------------------------------------------------------------
// option parsing — TypeError for a type mismatch, RangeError for a value out
// of range, the parameter named in every message
// ---------------------------------------------------------------------------

fn get_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<String>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let Some(s) = v.as_string(false) else {
        return Err(ctx.throw_type_error(&format!("`sandbox.{key}` must be a string")));
    };
    Ok(Some(s.content().encode_utf8(false).to_string()))
}

/// Build the policy from `sandbox`, or `None` when the option is absent.
///
/// `cwd` is the caller's top-level working directory, which is also where the
/// sandbox runs and the one place it may write. It is not a separate field on
/// `sandbox`: "where the command starts" is one concept, and giving it two
/// names is what produced a top-level `cwd` that was silently ignored. When it
/// is absent the current directory is used, matching what a plain spawn does.
///
/// Every rejection names the offending field and says what is wrong with it.
/// Nothing here degrades: an option the platform cannot enforce is refused
/// rather than dropped, so a caller never gets a sandbox weaker than the one
/// they asked for without being told.
pub(crate) fn get_sandbox_opt(
    ctx: &mut ScopedContext,
    opts: &Object,
    cwd: Option<&str>,
) -> Result<Option<Policy>, i32> {
    let v = opts.get_property_str(ctx, "sandbox")?;
    if v.is_undefined() {
        return Ok(None);
    }
    // Arrays are objects in JS — reject them with a clear message.
    if v.is_array(ctx)? {
        return Err(ctx.throw_type_error(
            "`sandbox` must be an object ({grants, denied, network}), not an array",
        ));
    }
    let Some(sandbox) = v.as_object() else {
        return Err(ctx.throw_type_error("`sandbox` must be an object"));
    };

    // Checked before anything else so an unusable machine gets told that,
    // rather than finding out after their paths have been resolved.
    if let Err(reason) = super::probe::probe() {
        return Err(ctx.throw_type_error(reason));
    }

    // No `cwd` means the current directory, so the sandbox writes where the
    // command would have run anyway.
    let working_dir = match cwd {
        Some(dir) => dir.to_owned(),
        None => std::env::current_dir()
            .map_err(|e| {
                ctx.throw_internal_error(&format!(
                    "`cwd` is unset and the current directory cannot be read: {e}"
                ))
            })?
            .to_str()
            .ok_or_else(|| {
                ctx.throw_type_error("`cwd` is unset and the current directory is not valid UTF-8")
            })?
            .to_owned(),
    };

    let grants = parse_grants(ctx, &sandbox)?;
    let denied = parse_denied(ctx, &sandbox)?;
    let network = parse_network(ctx, &sandbox)?;

    Policy::new(&working_dir, grants, denied, network)
        .map(Some)
        .map_err(|e| ctx.throw_type_error(&format!("invalid sandbox configuration: {e}")))
}

/// `grants`: [{path, access}] where access is "r" | "rw" | "rx" | "rwx".
fn parse_grants(ctx: &mut ScopedContext, sandbox: &Object) -> Result<Vec<(String, String)>, i32> {
    let v = sandbox.get_property_str(ctx, "grants")?;
    if v.is_undefined() {
        return Ok(Vec::new());
    }
    let arr = match Array::try_from(v) {
        Ok(a) => a,
        Err(_) => return Err(ctx.throw_type_error("`sandbox.grants` must be an array")),
    };
    let len = arr.get_length_prop(ctx)? as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let item = arr.get_property_u32(ctx, i as u32)?;
        let Some(g) = item.as_object() else {
            return Err(ctx.throw_type_error(&format!(
                "`sandbox.grants[{i}]` must be an object with `path` and `access`"
            )));
        };
        let path = get_str_prop(ctx, &g, "path")?.ok_or_else(|| {
            ctx.throw_type_error(&format!("`sandbox.grants[{i}].path` is required"))
        })?;
        let access = get_str_prop(ctx, &g, "access")?.unwrap_or_else(|| "r".to_owned());
        out.push((path, access));
    }
    Ok(out)
}

/// `denied`: [path, ...]
fn parse_denied(ctx: &mut ScopedContext, sandbox: &Object) -> Result<Vec<String>, i32> {
    let v = sandbox.get_property_str(ctx, "denied")?;
    if v.is_undefined() {
        return Ok(Vec::new());
    }
    let arr = match Array::try_from(v) {
        Ok(a) => a,
        Err(_) => return Err(ctx.throw_type_error("`sandbox.denied` must be an array of paths")),
    };
    let len = arr.get_length_prop(ctx)? as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let item = arr.get_property_u32(ctx, i as u32)?;
        let Some(s) = item.as_string(true) else {
            return Err(ctx.throw_type_error(&format!("`sandbox.denied[{i}]` must be a string")));
        };
        out.push(s.content().encode_utf8(false).to_string());
    }
    Ok(out)
}

/// `network`: "none" (default) | "host" | {allow: [...]}
fn parse_network(ctx: &mut ScopedContext, sandbox: &Object) -> Result<NetworkPolicy, i32> {
    let v = sandbox.get_property_str(ctx, "network")?;
    if v.is_undefined() {
        return Ok(NetworkPolicy::DenyAll);
    }
    if let Some(s) = v.as_string(false) {
        return match s.content().encode_utf8(false).as_ref() {
            "none" => Ok(NetworkPolicy::DenyAll),
            "host" => Ok(NetworkPolicy::AllowAll),
            _ => Err(ctx.throw_range_error("`sandbox.network` must be \"none\" or \"host\"")),
        };
    }
    let Some(o) = v.as_object() else {
        return Err(ctx.throw_range_error("`sandbox.network` must be \"none\" or \"host\""));
    };
    let allow_v = o.get_property_str(ctx, "allow")?;
    if !allow_v.is_undefined() {
        // A domain allowlist is enforced by a proxy running outside the
        // sandbox, which is not built yet. Denying everything would be
        // fail-closed, but the option would look accepted while blocking all
        // traffic, so it is refused instead.
        return Err(ctx.throw_type_error(
            "`sandbox.network: {allow: [...]}` is not implemented yet \u{2014} no proxy \
             runs, so every domain would be denied while the option looked accepted. \
             Use \"none\" or \"host\" until it lands.",
        ));
    }
    Err(ctx.throw_range_error(
        "`sandbox.network` object form takes {allow: [...]}, which is not implemented yet",
    ))
}

// ---------------------------------------------------------------------------
// registry of live sandboxed children (pid → control handle)
// ---------------------------------------------------------------------------

type SharedChild = Arc<super::child::ChildControl>;

/// Live sandboxed children keyed by pid. Global rather than thread-local:
/// the engine may invoke callbacks from different threads, matching how
/// spawn.rs keeps its CHILD_EXITS registry. Holds no GC nodes.
static SANDBOX_CHILDREN: LazyLock<Mutex<HashMap<i32, SharedChild>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn insert_sandbox_child(pid: i32, control: SharedChild) {
    SANDBOX_CHILDREN.lock().unwrap().insert(pid, control);
}

fn remove_sandbox_child(pid: i32) {
    SANDBOX_CHILDREN.lock().unwrap().remove(&pid);
}

fn get_shared_child(pid: i32) -> Option<SharedChild> {
    SANDBOX_CHILDREN.lock().unwrap().get(&pid).cloned()
}

// ---------------------------------------------------------------------------
// async path — Aike.spawn(..., {sandbox}) → Subprocess
// ---------------------------------------------------------------------------

pub(crate) fn spawn_async_sandboxed(
    ctx: &mut ScopedContext,
    policy: &Policy,
    cmd: String,
    args: Vec<String>,
    env: Option<HashMap<String, String>>,
    timeout_ms: Option<u64>,
    abort_on_parent_exit: bool,
) -> JSResult {
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let empty = HashMap::new();
    let mut child = super::spawn(policy, &cmd, &arg_refs, env.as_ref().unwrap_or(&empty))
        .map_err(|e| ctx.throw_internal_error(&format!("sandbox spawn failed: {e}")))?;

    let pid = child.pid;
    let stdin_val = match child.stdin.take() {
        Some(si) => {
            let fd = raw_fd(&si);
            JSStdioHandle::new_instance_pipe_write(ctx, pipe_bridge::bridge_writer(si), fd)
                .map(JSValue::from)
                .unwrap_or(JSValue::NULL)
        }
        None => JSValue::NULL,
    };
    let stdout_val = match child.stdout.take() {
        Some(so) => {
            let fd = raw_fd(&so);
            JSStdioHandle::new_instance_pipe_read(ctx, pipe_bridge::bridge_reader(so), fd)
                .map(JSValue::from)
                .unwrap_or(JSValue::NULL)
        }
        None => JSValue::NULL,
    };
    let stderr_val = match child.stderr.take() {
        Some(se) => {
            let fd = raw_fd(&se);
            JSStdioHandle::new_instance_pipe_read(ctx, pipe_bridge::bridge_reader(se), fd)
                .map(JSValue::from)
                .unwrap_or(JSValue::NULL)
        }
        None => JSValue::NULL,
    };

    let control = Arc::new(child.control.clone());
    insert_sandbox_child(pid, Arc::clone(&control));
    register_child_exit(pid);

    let slot_exit = PromiseResultSlot::<Result<i32, String>>::new();
    let prom_exit =
        Promise::create_with_result_slot(ctx, slot_exit.clone(), |cx, data| match data {
            Ok(code) => Ok(JSValue::from(code)),
            Err(e) => Err(cx.throw_internal_error(&e)),
        })?;
    let slot_signal = PromiseResultSlot::<Result<Option<String>, String>>::new();
    let prom_signal =
        Promise::create_with_result_slot(ctx, slot_signal.clone(), |cx, data| match data {
            Ok(Some(name)) => cx
                .create_string_from_utf8(name.into_bytes())
                .map(JSValue::make_string),
            Ok(None) => Ok(JSValue::NULL),
            Err(e) => Err(cx.throw_internal_error(&e)),
        })?;
    let slot_killed = PromiseResultSlot::<bool>::new();
    let prom_killed = Promise::create_with_result_slot(ctx, slot_killed.clone(), |_, killed| {
        Ok(JSValue::from(killed))
    })?;

    let mut result = ctx.create_base_object()?;
    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "sandboxed", JSValue::TRUE, JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdin", stdin_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stdout", stdout_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", stderr_val, JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", prom_exit.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", prom_signal.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "killedByTimeout", prom_killed.into(), JS_PROP_C_W_E)?;

    // kill(signal?) / isRunning() — resolve through the registry so they work
    // on the tree rather than on a bare pid.
    let kill_fn =
        ctx.create_native_function(Callback::Func(js_sandbox_child_kill), "kill", 1, 0)?;
    result.define_property_value_str(ctx, "kill", kill_fn.into(), JS_PROP_C_W_E)?;
    let is_running_fn = ctx.create_native_function(
        Callback::Func(js_sandbox_child_is_running),
        "isRunning",
        0,
        0,
    )?;
    result.define_property_value_str(ctx, "isRunning", is_running_fn.into(), JS_PROP_C_W_E)?;

    let shutdown_fn =
        ctx.create_native_function(Callback::Func(js_sandbox_child_shutdown), "shutdown", 1, 0)?;
    result.define_property_value_str(ctx, "shutdown", shutdown_fn.into(), JS_PROP_C_W_E)?;

    if abort_on_parent_exit {
        register_child(pid);
    }

    let killer = child.control.clone();
    tokio::task::spawn_blocking(move || {
        let killed = Arc::new(AtomicBool::new(false));
        if let Some(d) = timeout_ms {
            let killer = killer.clone();
            let killed = Arc::clone(&killed);
            tokio::task::spawn_blocking(move || {
                std::thread::sleep(Duration::from_millis(d));
                if killer.kill() {
                    killed.store(true, Ordering::Relaxed);
                }
            });
        }
        let r = child.wait();
        remove_sandbox_child(pid);
        signal_child_exit(pid);
        if abort_on_parent_exit {
            unregister_child(pid);
        }
        match r {
            Ok(res) => {
                let _ = slot_exit.commit(Ok(res.exit_code));
                let _ = slot_signal.commit(Ok(res
                    .signal
                    .and_then(|n| signal_name(n as u32))
                    .map(String::from)));
                let _ = slot_killed.commit(killed.load(Ordering::Relaxed) || res.killed_by_timeout);
            }
            Err(e) => {
                let _ = slot_exit.commit(Err(e.clone()));
                let _ = slot_signal.commit(Err(e));
                let _ = slot_killed.commit(killed.load(Ordering::Relaxed));
            }
        }
    });

    Ok(result.into())
}

// ---------------------------------------------------------------------------
// sync path — Aike.spawnSync / Aike.sh(..., {sandbox})
// ---------------------------------------------------------------------------

struct SandboxedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    result: super::child::WaitResult,
}

/// Drain the pipes and wait. Runs on the collector thread.
fn collect_sandbox_output(
    mut child: SpawnedChild,
    input: Option<Vec<u8>>,
) -> Result<SandboxedOutput, String> {
    if let Some(data) = input
        && let Some(mut si) = child.stdin.take()
    {
        use std::io::Write;
        let _ = si.write_all(&data);
        let _ = si.flush();
    }
    drop(child.stdin.take()); // EOF

    let mut stdout = Vec::new();
    if let Some(mut r) = child.stdout.take() {
        use std::io::Read;
        let _ = r.read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(mut r) = child.stderr.take() {
        use std::io::Read;
        let _ = r.read_to_end(&mut stderr);
    }
    let result = child.wait()?;
    Ok(SandboxedOutput {
        stdout,
        stderr,
        result,
    })
}

/// Run `policy`-confined `program` to completion and collect its output.
///
/// Shared by `spawnSync` and `Aike.sh`, which differ only in how they shape
/// the result (`Uint8Array` versus string). `Aike.sh` used to have its own
/// `Command` and no sandbox path at all, which meant the option was silently
/// ignored there — the hole an agent hits first, since `sh` is the natural way
/// to run a command.
pub(crate) fn run_sandboxed_sync(
    policy: &Policy,
    program: &str,
    args: &[&str],
    env: Option<HashMap<String, String>>,
    input: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
) -> (i32, Vec<u8>, Vec<u8>, bool) {
    use std::sync::mpsc;

    let empty = HashMap::new();
    let (pid, child) = match super::spawn(policy, program, args, env.as_ref().unwrap_or(&empty)) {
        Ok(c) => (c.pid, c),
        Err(e) => return (-1, Vec::new(), e.into_bytes(), false),
    };
    let control = child.control.clone();

    let (tx, rx) = mpsc::channel::<Result<SandboxedOutput, String>>();
    std::thread::spawn(move || {
        let _ = tx.send(collect_sandbox_output(child, input));
    });

    // On overrun kill the tree; killing closes the pipes so the collector
    // finishes and the channel delivers.
    let (out, killed_by_timeout) = match timeout_ms {
        Some(ms) => match rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(r) => (r, false),
            Err(_) => {
                control.kill();
                let r = rx.recv().expect("collector finishes after kill");
                (r, true)
            }
        },
        None => (rx.recv().expect("collector must send"), false),
    };
    remove_sandbox_child(pid);

    match out {
        Ok(o) => (o.result.exit_code, o.stdout, o.stderr, killed_by_timeout),
        Err(e) => (-1, Vec::new(), e.into_bytes(), killed_by_timeout),
    }
}

pub(crate) fn spawn_sync_sandboxed(
    ctx: &mut ScopedContext,
    policy: &Policy,
    cmd: String,
    args: Vec<String>,
    env: Option<HashMap<String, String>>,
    input: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
) -> JSResult {
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    // Resolve up front so a bad program name is an exception, not an exit code.
    super::spawn(
        policy,
        &cmd,
        &arg_refs,
        env.as_ref().unwrap_or(&HashMap::new()),
    )
    .map_err(|e| ctx.throw_internal_error(&format!("sandbox spawn failed: {e}")))?;

    let (exit_code, stdout, stderr, killed) =
        run_sandboxed_sync(policy, &cmd, &arg_refs, env, input, timeout_ms);

    let mut result = ctx.create_base_object()?;
    let ta_out = TypedArray::create_from_vec_u8(ctx, stdout)?;
    let ta_err = TypedArray::create_from_vec_u8(ctx, stderr)?;
    result.define_property_value_str(ctx, "sandboxed", JSValue::TRUE, JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdout", ta_out.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", ta_err.into(), JS_PROP_C_W_E)?;
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
        JSValue::from(killed),
        JS_PROP_C_W_E,
    )?;
    Ok(result.into())
}

// ---------------------------------------------------------------------------
// tree-aware control functions for sandboxed Subprocess objects
// ---------------------------------------------------------------------------

/// kill(signal?) — signal the sandboxed child tree. Defaults to SIGKILL.
fn js_sandbox_child_kill(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(ctx, &cc);
    let Some(shared) = get_shared_child(pid) else {
        return Ok(JSValue::FALSE);
    };
    if !shared.is_running() {
        return Ok(JSValue::FALSE);
    }

    #[cfg(unix)]
    {
        let sig = if cc.length() > 0 && !cc.arg(0).is_undefined() {
            let arg = cc.arg(0);
            if let Some(s) = arg.as_string(true) {
                let name = s.content().encode_utf8(false);
                crate::aike::signal::signal_number(&name)
                    .map(|n| n as i32)
                    .unwrap_or(libc::SIGKILL)
            } else {
                arg.to_number(ctx)?.to_i32()
            }
        } else {
            libc::SIGKILL
        };
        Ok(JSValue::from(shared.send_signal(sig)))
    }

    #[cfg(windows)]
    {
        let _ = (ctx, cc);
        Ok(JSValue::from(shared.kill()))
    }
}

/// shutdown(options?) — close stdin (EOF), then termSignal, then SIGKILL, each
/// stage bounded by its timeout. Resolves with the stage the child exited at:
/// "eof" | "term" | "kill" | "timed_out" | "already_closed".
fn js_sandbox_child_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    fn resolved_stage(ctx: &mut ScopedContext, stage: &'static str) -> JSResult {
        let (promise, tx) = Promise::create_with_channel::<String>(ctx, |cx, stage| {
            cx.create_string_from_utf8(stage.as_bytes().to_vec())
                .map(JSValue::make_string)
        })?;
        let _ = tx.send(stage.to_string());
        Ok(promise.into())
    }

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
            match s.content().encode_utf8(false).as_ref() {
                "SIGTERM" | "SIGINT" => {
                    term_sig = crate::aike::signal::signal_number(&s.content().encode_utf8(false))
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

    // Claim the child's exit state: absent → never spawned or already reaped;
    // already shutting down → report without re-running the ladder.
    let rx = {
        let mut map = crate::aike::spawn::CHILD_EXITS.lock().unwrap();
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

    // Close stdin so the child sees EOF (dropping the handle closes the fd).
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

fn js_sandbox_child_is_running(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(ctx, &cc);
    let Some(shared) = get_shared_child(pid) else {
        return Ok(JSValue::FALSE);
    };
    Ok(JSValue::from(shared.is_running()))
}

// ---------------------------------------------------------------------------
// availability
// ---------------------------------------------------------------------------

/// sandboxSupport() — can this machine run a sandbox, and if not, why.
///
/// Exists so a script can find out before offering the option, rather than
/// having the first spawn fail. `reason` is the same message a failing spawn
/// would throw.
pub(crate) fn js_sandbox_support(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    let mut obj = ctx.create_base_object()?;
    let backend = match super::probe::backend_name() {
        Some(b) => ctx
            .create_string_from_utf8(b.as_bytes().to_vec())
            .map(JSValue::make_string)?,
        None => JSValue::NULL,
    };
    match super::probe::probe() {
        Ok(()) => {
            obj.define_property_value_str(ctx, "available", JSValue::TRUE, JS_PROP_ENUMERABLE)?;
            obj.define_property_value_str(ctx, "backend", backend, JS_PROP_ENUMERABLE)?;
            obj.define_property_value_str(ctx, "reason", JSValue::NULL, JS_PROP_ENUMERABLE)?;
        }
        Err(reason) => {
            obj.define_property_value_str(ctx, "available", JSValue::FALSE, JS_PROP_ENUMERABLE)?;
            obj.define_property_value_str(ctx, "backend", JSValue::NULL, JS_PROP_ENUMERABLE)?;
            let s = ctx.create_string_from_utf8(reason.as_bytes().to_vec())?;
            obj.define_property_value_str(
                ctx,
                "reason",
                JSValue::make_string(s),
                JS_PROP_ENUMERABLE,
            )?;
        }
    }
    Ok(obj.into())
}
