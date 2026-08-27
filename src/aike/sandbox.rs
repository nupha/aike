// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `Aike.spawn(..., { sandbox: {...} })` — optional sandboxing for child
//! processes, backed by the local nanosandbox fork's detached spawn.
//!
//! When the option is present, the command runs through
//! [`nanosandbox::Sandbox::spawn`]; its blocking stdio pipes are bridged into
//! tokio (`pipe_bridge`) so the regular Subprocess surface — streaming
//! handles, pid-driven kill/isRunning, exit promises — stays identical.
//! Without the option, spawn behaves exactly as before.

#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, atomic::{AtomicBool, Ordering}},
    time::Duration,
};

use nanosandbox::{ExecutionResult, Sandbox};
use zuqe::{
    Array, CallContext, Callback, IntOrFloat, JS_GPN_ENUM_ONLY, JS_GPN_STR_OR_INT_MASK,
    JS_PROP_C_W_E, JS_PROP_ENUMERABLE, JSResult, JSValue, Object, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
};

use super::{
    JSStdioHandle, pipe_bridge,
    signal::signal_name,
    spawn::{
        CHILD_EXITS, get_pos_ms_prop, pid_from_this, register_child, register_child_exit,
        shutdown_ladder, signal_child_exit, unregister_child,
    },
};

// ---------------------------------------------------------------------------
// strict option-parsing helpers (TypeError for type mismatches, RangeError
// for out-of-range values, parameter names in every message)
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

/// Positive-integer property.
fn get_pos_int_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<u64>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`sandbox.{key}` must be a number")));
    }
    match v.to_number(ctx)? {
        IntOrFloat::Int64(i) if i > 0 => Ok(Some(i as u64)),
        IntOrFloat::Float64(d) if d > 0.0 && d.fract() == 0.0 => Ok(Some(d as u64)),
        _ => Err(ctx.throw_range_error(&format!("`sandbox.{key}` must be a positive integer"))),
    }
}

/// Positive float property (> 0), fractions allowed (e.g. cpuLimit).
fn get_pos_float_prop(
    ctx: &mut ScopedContext,
    obj: &Object,
    key: &str,
) -> Result<Option<f64>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`sandbox.{key}` must be a number")));
    }
    let n = v.to_number(ctx)?.to_f64();
    if n <= 0.0 {
        return Err(ctx.throw_range_error(&format!("`sandbox.{key}` must be > 0")));
    }
    Ok(Some(n))
}

fn parse_string_array(
    ctx: &mut ScopedContext,
    val: &JSValue,
    name: &str,
) -> Result<Vec<String>, i32> {
    let arr = Array::try_from(*val)
        .map_err(|_| ctx.throw_type_error(&format!("`{name}` must be an array")))?;
    let len = arr.get_length_prop(ctx)? as usize;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let s = arr
            .get_property_u32(ctx, i as u32)?
            .as_string(false)
            .ok_or_else(|| ctx.throw_type_error(&format!("`{name}` must contain only strings")))?
            .content()
            .encode_utf8(false)
            .to_string();
        out.push(s);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// sandbox construction from JS options
// ---------------------------------------------------------------------------

/// Read `options.sandbox`. Returns `Ok(None)` when absent so plain spawns are
/// untouched; rejects arrays and non-objects explicitly.
pub(crate) fn get_sandbox_opt(
    ctx: &mut ScopedContext,
    opts: &Object,
    env: Option<HashMap<String, String>>,
) -> Result<Option<Sandbox>, i32> {
    let v = opts.get_property_str(ctx, "sandbox")?;
    if v.is_undefined() {
        return Ok(None);
    }
    // Arrays are objects in JS — reject them with a clear message.
    if v.is_array(ctx)? {
        return Err(
            ctx.throw_type_error("`sandbox` must be an object ({cwd, mounts, ...}), not an array")
        );
    }
    let Some(opts) = v.as_object() else {
        return Err(ctx.throw_type_error("`sandbox` must be an object"));
    };

    // cwd — required, must be an existing directory; canonicalized so the
    // macOS Seatbelt subpath rule matches real paths even under symlinks.
    let cwd = get_str_prop(ctx, &opts, "cwd")?.ok_or_else(|| {
        ctx.throw_type_error("`sandbox.cwd` is required and must be an existing directory")
    })?;
    let cwd_path = std::path::Path::new(&cwd);
    if !cwd_path.is_dir() {
        return Err(ctx.throw_range_error(&format!(
            "`sandbox.cwd` must be an existing directory: {cwd}"
        )));
    }
    let cwd_abs = std::fs::canonicalize(cwd_path).unwrap_or_else(|_| cwd_path.to_path_buf());

    let mut b = Sandbox::builder().working_dir(cwd_abs);

    // mounts: Record<source, {target?, writable?}> — target omitted means
    // same-path mount, writable defaults to false (ro).
    let mounts_v = opts.get_property_str(ctx, "mounts")?;
    if !mounts_v.is_undefined() {
        if mounts_v.is_array(ctx)? {
            return Err(ctx.throw_type_error(
                "`sandbox.mounts` must be an object keyed by host path, not an array",
            ));
        }
        let Some(mut mounts) = mounts_v.as_object() else {
            return Err(ctx.throw_type_error("`sandbox.mounts` must be an object"));
        };
        let pks = mounts.get_own_property_names(ctx, JS_GPN_ENUM_ONLY | JS_GPN_STR_OR_INT_MASK)?;
        for pk in pks.iter() {
            let source = pk
                .atom
                .to_value(ctx)?
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
            let Some(m) = mounts.get_property(ctx, pk.atom)?.as_object() else {
                return Err(
                    ctx.throw_type_error(&format!("`sandbox.mounts[{source}]` must be an object"))
                );
            };

            let writable_v = m.get_property_str(ctx, "writable")?;
            let writable = if writable_v.is_undefined() {
                false
            } else if writable_v.is_bool() {
                writable_v.to_bool()?
            } else {
                return Err(ctx.throw_type_error(&format!(
                    "`sandbox.mounts[{source}].writable` must be a boolean"
                )));
            };

            let target = get_str_prop(ctx, &m, "target")?.unwrap_or_else(|| source.clone());
            b = b.mount(
                PathBuf::from(source),
                PathBuf::from(target),
                if writable {
                    nanosandbox::Permission::ReadWrite
                } else {
                    nanosandbox::Permission::ReadOnly
                },
            );
        }
    }

    // tmpfs: [{path, size}]
    let tmpfs_v = opts.get_property_str(ctx, "tmpfs")?;
    if !tmpfs_v.is_undefined() {
        let arr = Array::try_from(tmpfs_v)
            .map_err(|_| ctx.throw_type_error("`sandbox.tmpfs` must be an array"))?;
        let len = arr.get_length_prop(ctx)? as usize;
        for i in 0..len {
            let item = arr.get_property_u32(ctx, i as u32)?;
            let Some(m) = item.as_object() else {
                return Err(ctx.throw_type_error("`sandbox.tmpfs[i]` must be an object"));
            };
            let path = get_str_prop(ctx, &m, "path")?
                .ok_or_else(|| ctx.throw_type_error("`sandbox.tmpfs[i].path` must be a string"))?;
            let size = get_pos_int_prop(ctx, &m, "size")?.ok_or_else(|| {
                ctx.throw_range_error("`sandbox.tmpfs[i].size` must be a positive integer")
            })?;
            b = b.tmpfs(PathBuf::from(path), size);
        }
    }

    if let Some(v) = get_str_prop(ctx, &opts, "rootfs")? {
        b = b.rootfs(PathBuf::from(v));
    }
    if let Some(v) = get_pos_int_prop(ctx, &opts, "memoryLimit")? {
        b = b.memory_limit(v);
    }
    if let Some(v) = get_pos_float_prop(ctx, &opts, "cpuLimit")? {
        b = b.cpu_limit(v);
    }
    if let Some(ms) = get_pos_int_prop(ctx, &opts, "wallTimeLimitMs")? {
        b = b.wall_time_limit(Duration::from_millis(ms));
    }
    if let Some(ms) = get_pos_int_prop(ctx, &opts, "cpuTimeLimitMs")? {
        b = b.cpu_time_limit(Duration::from_millis(ms));
    }
    if let Some(n) = get_pos_int_prop(ctx, &opts, "maxPids")? {
        b = b.max_pids(
            u32::try_from(n)
                .map_err(|_| ctx.throw_range_error("`sandbox.maxPids` exceeds u32 range"))?,
        );
    }
    if let Some(v) = get_pos_int_prop(ctx, &opts, "maxFileSize")? {
        b = b.max_file_size(v);
    }
    if let Some(n) = get_pos_int_prop(ctx, &opts, "maxOpenFiles")? {
        b = b.max_open_files(
            u32::try_from(n)
                .map_err(|_| ctx.throw_range_error("`sandbox.maxOpenFiles` exceeds u32 range"))?,
        );
    }

    // network: "none" | "host" | {allow: string[]}
    let net_v = opts.get_property_str(ctx, "network")?;
    if !net_v.is_undefined() {
        if let Some(s) = net_v.as_string(false) {
            match s.content().encode_utf8(false).as_ref() {
                "none" => b = b.no_network(),
                "host" => b = b.host_network(),
                _ => {
                    return Err(ctx.throw_range_error(
                        "`sandbox.network` must be \"none\", \"host\" or {allow: [...]}",
                    ));
                }
            }
        } else if let Some(o) = net_v.as_object() {
            let allow_v = o.get_property_str(ctx, "allow")?;
            let allow = parse_string_array(ctx, &allow_v, "sandbox.network.allow")?;
            let refs: Vec<&str> = allow.iter().map(|s| s.as_str()).collect();
            b = b.allow_network(&refs);
        } else {
            return Err(ctx.throw_range_error(
                "`sandbox.network` must be \"none\", \"host\" or {allow: [...]}",
            ));
        }
    }

    // seccompProfile: named profile | custom syscall list
    let seccomp_v = opts.get_property_str(ctx, "seccompProfile")?;
    if !seccomp_v.is_undefined() {
        if let Some(s) = seccomp_v.as_string(false) {
            match s.content().encode_utf8(false).as_ref() {
                "disabled" => b = b.seccomp_profile(nanosandbox::SeccompProfile::Disabled),
                "strict" => b = b.seccomp_profile(nanosandbox::SeccompProfile::Strict),
                "standard" => b = b.seccomp_profile(nanosandbox::SeccompProfile::Standard),
                "permissive" => b = b.seccomp_profile(nanosandbox::SeccompProfile::Permissive),
                _ => {
                    return Err(ctx.throw_range_error(
                        "`sandbox.seccompProfile` must be \"disabled\"|\"strict\"|\"standard\"|\"permissive\" or a syscall list",
                    ));
                }
            }
        } else if seccomp_v.as_object().is_some() {
            let syscalls = parse_string_array(ctx, &seccomp_v, "sandbox.seccompProfile")?;
            b = b.seccomp_profile(nanosandbox::SeccompProfile::Custom(syscalls));
        } else {
            return Err(ctx.throw_range_error(
                "`sandbox.seccompProfile` must be \"disabled\"|\"strict\"|\"standard\"|\"permissive\" or a syscall list",
            ));
        }
    }

    if let Some(v) = get_pos_int_prop(ctx, &opts, "uid")? {
        b = b.uid(
            u32::try_from(v)
                .map_err(|_| ctx.throw_range_error("`sandbox.uid` exceeds u32 range"))?,
        );
    }
    if let Some(v) = get_pos_int_prop(ctx, &opts, "gid")? {
        b = b.gid(
            u32::try_from(v)
                .map_err(|_| ctx.throw_range_error("`sandbox.gid` exceeds u32 range"))?,
        );
    }

    // env: the single env knob (SpawnOptions.env), forwarded from the caller.
    // Replaces the entire sandbox env, matching plain-spawn semantics.
    if let Some(env_map) = env {
        b = b.envs(env_map);
    }

    if let Some(v) = get_str_prop(ctx, &opts, "hostname")? {
        b = b.hostname(v);
    }

    let sb = b
        .build()
        .map_err(|e| ctx.throw_type_error(&format!("invalid sandbox configuration: {e}")))?;
    Ok(Some(sb))
}

// ---------------------------------------------------------------------------
// registry of live sandboxed children (pid → shared handle)
//
// Stores no GC nodes (pure Rust handles), so no GC trace hook is needed.
// ---------------------------------------------------------------------------

type SharedChild = Arc<nanosandbox::SandboxKiller>;

/// Live sandboxed children keyed by pid. Global (not thread-local): the
/// engine may invoke callbacks from different threads, matching how
/// spawn.rs keeps its CHILD_EXITS registry. Stores no GC nodes.
static SANDBOX_CHILDREN: LazyLock<Mutex<HashMap<i32, SharedChild>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn insert_sandbox_child(pid: i32, killer: nanosandbox::SandboxKiller) {
    SANDBOX_CHILDREN
        .lock()
        .unwrap()
        .insert(pid, Arc::new(killer));
}

fn remove_sandbox_child(pid: i32) {
    SANDBOX_CHILDREN.lock().unwrap().remove(&pid);
}

fn get_shared_child(pid: i32) -> Option<SharedChild> {
    SANDBOX_CHILDREN.lock().unwrap().get(&pid).cloned()
}

// ---------------------------------------------------------------------------
// async path — Aike.spawn(..., {sandbox}) → Subprocess-shaped object
// ---------------------------------------------------------------------------

pub(crate) fn spawn_async_sandboxed(
    ctx: &mut ScopedContext,
    sb: Sandbox,
    cmd: String,
    args: Vec<String>,
    timeout_ms: Option<u64>,
    abort_on_parent_exit: bool,
) -> JSResult {
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut child = sb
        .spawn(&cmd, &arg_refs)
        .map_err(|e| ctx.throw_internal_error(&format!("sandbox spawn failed: {e}")))?;

    let pid = child.id() as i32;
    let stdin_val = match child.take_stdin() {
        Some(si) => {
            #[cfg(unix)]
            let fd = si.as_raw_fd();
            #[cfg(not(unix))]
            let fd = -1;
            JSStdioHandle::new_instance_pipe_write(ctx, pipe_bridge::bridge_writer(si), fd)
                .map(JSValue::from)
                .unwrap_or(JSValue::NULL)
        }
        None => JSValue::NULL,
    };
    let stdout_val = match child.take_stdout() {
        Some(so) => {
            #[cfg(unix)]
            let fd = so.as_raw_fd();
            #[cfg(not(unix))]
            let fd = -1;
            JSStdioHandle::new_instance_pipe_read(ctx, pipe_bridge::bridge_reader(so), fd)
                .map(JSValue::from)
                .unwrap_or(JSValue::NULL)
        }
        None => JSValue::NULL,
    };
    let stderr_val = match child.take_stderr() {
        Some(se) => {
            #[cfg(unix)]
            let fd = se.as_raw_fd();
            #[cfg(not(unix))]
            let fd = -1;
            JSStdioHandle::new_instance_pipe_read(ctx, pipe_bridge::bridge_reader(se), fd)
                .map(JSValue::from)
                .unwrap_or(JSValue::NULL)
        }
        None => JSValue::NULL,
    };

    let killer = child.killer();
    insert_sandbox_child(pid, killer.clone());
    register_child_exit(pid);

    // exitCode / signalCode / killedByTimeout promises — same shape as the
    // regular async path, resolved by a blocking wait on the worker pool.
    // Use PromiseResultSlot instead of channels to avoid disconnect races.
    use zuqe::builtins::promise::PromiseResultSlot;
    let slot_exit =
        PromiseResultSlot::<Result<i32, String>>::new();
    let prom_exit =
        Promise::create_with_result_slot(ctx, slot_exit.clone(), |cx, data| match data {
            Ok(code) => Ok(JSValue::from(code)),
            Err(e) => Err(cx.throw_internal_error(&e)),
        })?;
    let slot_signal =
        PromiseResultSlot::<Result<Option<String>, String>>::new();
    let prom_signal =
        Promise::create_with_result_slot(ctx, slot_signal.clone(), |cx, data| match data {
            Ok(Some(name)) => cx
                .create_string_from_utf8(name.into_bytes())
                .map(JSValue::make_string),
            Ok(None) => Ok(JSValue::NULL),
            Err(e) => Err(cx.throw_internal_error(&e)),
        })?;
    let slot_killed = PromiseResultSlot::<bool>::new();
    let prom_killed =
        Promise::create_with_result_slot(ctx, slot_killed.clone(), |_, killed| {
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

    // kill(signal?) / isRunning() — tree-aware implementations that resolve
    // the sandboxed child via the registry instead of signaling a bare pid.
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

    // shutdown(options?) — graceful close (EOF → term → SIGKILL), same
    // semantics as the non-sandbox path. Reuses CHILD_EXITS for exit
    // notification and shutdown_ladder for the actual ladder.
    let shutdown_fn =
        ctx.create_native_function(Callback::Func(js_sandbox_child_shutdown), "shutdown", 1, 0)?;
    result.define_property_value_str(ctx, "shutdown", shutdown_fn.into(), JS_PROP_C_W_E)?;

    if abort_on_parent_exit {
        register_child(pid);
    }

    // Waiter task: owns the child. child.wait() blocks on wait4 and
    // returns the real exit status when sandbox-exec finally exits (on
    // macOS, sandbox-exec is a persistent daemon that stays alive after
    // exec'ing the command; it only exits when killed by SIGKILL).
    tokio::task::spawn_blocking(move || {
        let killed = Arc::new(AtomicBool::new(false));
        if let Some(d) = timeout_ms {
            // Spawn a timer that kills the tree after the deadline.
            let killer_clone = killer.clone();
            let killed_clone = Arc::clone(&killed);
            tokio::task::spawn_blocking(move || {
                std::thread::sleep(Duration::from_millis(d));
                if killer_clone.kill() {
                    killed_clone.store(true, Ordering::Relaxed);
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
                let msg = e.to_string();
                let _ = slot_exit.commit(Err(msg.clone()));
                let _ = slot_signal.commit(Err(msg));
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
    result: ExecutionResult,
}

/// Drain remaining pipes and wait. Runs in the collector thread.
fn collect_sandbox_output(
    mut child: nanosandbox::SandboxChild,
    input: Option<Vec<u8>>,
) -> Result<SandboxedOutput, String> {
    if let Some(data) = input {
        if let Some(mut si) = child.take_stdin() {
            use std::io::Write;
            let _ = si.write_all(&data);
            let _ = si.flush();
        }
    }
    drop(child.take_stdin()); // EOF

    let mut stdout = Vec::new();
    if let Some(mut r) = child.take_stdout() {
        use std::io::Read;
        let _ = r.read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(mut r) = child.take_stderr() {
        use std::io::Read;
        let _ = r.read_to_end(&mut stderr);
    }
    let result = child.wait().map_err(|e| e.to_string())?;
    Ok(SandboxedOutput {
        stdout,
        stderr,
        result,
    })
}

pub(crate) fn spawn_sync_sandboxed(
    ctx: &mut ScopedContext,
    sb: Sandbox,
    cmd: String,
    args: Vec<String>,
    input: Option<Vec<u8>>,
    timeout_ms: Option<u64>,
) -> JSResult {
    use std::sync::mpsc;

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let child = sb
        .spawn(&cmd, &arg_refs)
        .map_err(|e| ctx.throw_internal_error(&format!("sandbox spawn failed: {e}")))?;
    let pid = child.id() as i32;
    let killer = child.killer();
    insert_sandbox_child(pid, killer.clone());

    let (tx, rx) = mpsc::channel::<Result<SandboxedOutput, String>>();
    std::thread::spawn(move || {
        let _ = tx.send(collect_sandbox_output(child, input));
    });

    // Timeout ladder mirrors collect_with_timeout: on overrun kill the tree
    // via the killer; killing closes the pipes so the collector finishes.
    let (out, killed_by_timeout) = match timeout_ms {
        Some(ms) => match rx.recv_timeout(Duration::from_millis(ms)) {
            Ok(r) => (r, false),
            Err(_) => {
                killer.kill();
                let r = rx.recv().expect("collector finishes after kill");
                (r, true)
            }
        },
        None => (rx.recv().expect("collector must send"), false),
    };
    remove_sandbox_child(pid);

    build_sandbox_sync_result(ctx, pid, out, killed_by_timeout)
}

fn build_sandbox_sync_result(
    ctx: &mut ScopedContext,
    pid: i32,
    out: Result<SandboxedOutput, String>,
    killed_by_timeout: bool,
) -> JSResult {
    let out = match out {
        Ok(o) => o,
        Err(e) => return Err(ctx.throw_internal_error(&e)),
    };
    let exit_code = out.result.exit_code;
    let signal = out.result.signal;
    let killed = killed_by_timeout || out.result.killed_by_timeout;
    let signal_str = signal
        .and_then(|n| signal_name(n as u32))
        .and_then(|s| {
            ctx.create_string_from_utf8(s.as_bytes().to_vec())
                .ok()
                .map(JSValue::make_string)
        })
        .unwrap_or(JSValue::NULL);

    let mut result = ctx.create_base_object()?;
    let ta_out = TypedArray::create_from_vec_u8(ctx, out.stdout)?;
    let ta_err = TypedArray::create_from_vec_u8(ctx, out.stderr)?;
    result.define_property_value_str(ctx, "pid", JSValue::from(pid), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "sandboxed", JSValue::TRUE, JS_PROP_ENUMERABLE)?;
    result.define_property_value_str(ctx, "stdout", ta_out.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "stderr", ta_err.into(), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "exitCode", JSValue::from(exit_code), JS_PROP_C_W_E)?;
    result.define_property_value_str(ctx, "signalCode", signal_str, JS_PROP_C_W_E)?;
    result.define_property_value_str(
        ctx,
        "success",
        JSValue::from(exit_code == 0 && signal.is_none()),
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
// tree-aware child control functions for sandboxed Subprocess objects
// ---------------------------------------------------------------------------

/// kill(signal?) — send a signal to the sandboxed child tree.
/// Default signal is SIGKILL (matching the hard-kill semantics of the
/// sandbox). On Unix, accepts a signal name string or number.
///
/// NOTE (shutdown + sandbox + macOS): sandbox-exec does not forward signals
/// to the exec'd command. kill("SIGTERM") reaches sandbox-exec but the
/// inner process exits cleanly (code=0) without receiving the signal.
/// Use kill() (SIGKILL) for reliable termination on macOS.
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
                super::signal::signal_number(&name)
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
        let _ = ctx;
        let _ = cc;
        Ok(JSValue::from(shared.kill()))
    }
}

/// shutdown(options?) — gracefully shut the child down:
/// close stdin (EOF), then termSignal, then SIGKILL — each stage bounded
/// by its timeout. Resolves with the stage at which the child exited:
/// "eof" | "term" | "kill" | "timed_out" | "already_closed".
///
/// NOTE (shutdown + sandbox + macOS): On macOS, sandbox-exec does not
/// forward SIGTERM to the exec'd command, so the "term" stage is
/// effectively unreachable — the ladder always escalates to "kill".
/// The "eof" stage still works (stdin close propagates via pipe).
fn js_sandbox_child_shutdown(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    /// Resolve a promise to a fixed stage string.
    fn resolved_stage(ctx: &mut ScopedContext, stage: &'static str) -> JSResult {
        let (promise, tx) = Promise::create_with_channel::<String>(ctx, |cx, stage| {
            cx.create_string_from_str(&stage).map(JSValue::make_string)
        })?;
        let _ = tx.send(stage.to_string());
        Ok(promise.into())
    }

    // Strict option parsing (defaults mirror the non-sandbox path).
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
                    term_sig = super::signal::signal_number(&name)
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

    // Claim the child's exit state: absent → never spawned by us or already
    // reaped; already shutting down → report without re-running the ladder.
    let rx = {
        let mut map = super::spawn::CHILD_EXITS.lock().unwrap();
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
        cx.create_string_from_str(&stage).map(JSValue::make_string)
    })?;

    tokio::spawn(async move {
        let stage = shutdown_ladder(pid, rx, eof_ms, term_ms, kill_ms, term_sig).await;
        let _ = zuqe::PollSender::from(tx).send_async(stage.to_string()).await;
    });

    Ok(promise.into())
}

fn js_sandbox_child_is_running(_ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pid = pid_from_this(_ctx, &cc);
    let Some(shared) = get_shared_child(pid) else {
        return Ok(JSValue::FALSE);
    };
    Ok(JSValue::from(shared.is_running()))
}
