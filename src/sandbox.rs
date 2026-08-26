// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `sandbox` module — secure process execution backed by the `nanosandbox`
//! crate (namespaces/cgroups/seccomp on Linux, sandbox-exec on macOS, Job
//! Objects on Windows).
//!
//! The Rust builder chain is collapsed into a single options object:
//!
//! ```js
//! import { Sandbox } from "sandbox";
//! const sb = new Sandbox({ memoryLimit: 512 * 1024 * 1024 });
//! const r = await sb.run("echo", ["hi"]);
//! ```

use std::{path::PathBuf, sync::Arc, time::Duration};

use linkme::distributed_slice;
use zuqe::{
    Array, CallContext, ClassCtorDef, ClassID, FieldDef, JSResult, JSValue, NativeObject, Object,
    Runtime, ScopedContext,
    builtins::{promise::Promise, typed_array::TypedArray},
    def_native_object,
};

use crate::USER_CLASS_DEFS;

// ---------------------------------------------------------------------------
// strict option-parsing helpers
// ---------------------------------------------------------------------------

fn get_str_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<String>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    let Some(s) = v.as_string(false) else {
        return Err(ctx.throw_type_error(&format!("`{key}` must be a string")));
    };
    Ok(Some(s.content().encode_utf8(false).to_string()))
}

/// Positive-integer property: TypeError for non-numbers, RangeError for
/// non-positive or fractional values.
fn get_pos_int_prop(ctx: &mut ScopedContext, obj: &Object, key: &str) -> Result<Option<u64>, i32> {
    let v = obj.get_property_str(ctx, key)?;
    if v.is_undefined() {
        return Ok(None);
    }
    if !v.is_number() {
        return Err(ctx.throw_type_error(&format!("`{key}` must be a number")));
    }
    match v.to_number(ctx)? {
        zuqe::IntOrFloat::Int64(i) if i > 0 => Ok(Some(i as u64)),
        zuqe::IntOrFloat::Float64(d) if d > 0.0 && d.fract() == 0.0 => Ok(Some(d as u64)),
        _ => Err(ctx.throw_range_error(&format!("`{key}` must be a positive integer"))),
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
        return Err(ctx.throw_type_error(&format!("`{key}` must be a number")));
    }
    let n = v.to_number(ctx)?.to_f64();
    if n <= 0.0 {
        return Err(ctx.throw_range_error(&format!("`{key}` must be > 0")));
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
// Sandbox class
// ---------------------------------------------------------------------------

pub(crate) struct SandboxPayload(Arc<nanosandbox::Sandbox>);

fn js_sandbox_constructor(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let mut b = nanosandbox::Sandbox::builder();

    // options — required: `cwd` is mandatory (nanosandbox's own default "/"
    // would whitelist the whole filesystem for writes on macOS).
    if cc.length() == 0 || cc.arg(0).is_undefined() {
        return Err(ctx.throw_type_error("Sandbox requires an options object with `cwd`"));
    }
    let Some(opts) = cc.arg(0).as_object() else {
        return Err(ctx.throw_type_error("Sandbox options must be an object"));
    };

    // cwd — required, must be an existing directory; canonicalized so the
    // macOS Seatbelt subpath rule matches real paths even under symlinks
    // (e.g. /tmp → /private/tmp).
    let cwd = get_str_prop(ctx, &opts, "cwd")?
        .ok_or_else(|| ctx.throw_type_error("`cwd` is required and must be an existing directory"))?;
    let cwd_path = std::path::Path::new(&cwd);
    if !cwd_path.is_dir() {
        return Err(
            ctx.throw_range_error(&format!("`cwd` must be an existing directory: {cwd}"))
        );
    }
    let cwd_abs = std::fs::canonicalize(cwd_path)
        .unwrap_or_else(|_| cwd_path.to_path_buf());
    b = b.working_dir(cwd_abs);

    // mounts: Record<source, {target?, writable?}> — key is the host path;
    // target omitted means same-path mount, writable defaults to false (ro).
    let mounts_v = opts.get_property_str(ctx, "mounts")?;
    if !mounts_v.is_undefined() {
        // Arrays are objects in JS — reject them explicitly so a stray
        // legacy-style array fails with a clear message.
        if mounts_v.is_array(ctx)? {
            return Err(ctx.throw_type_error(
                "`mounts` must be an object keyed by host path, not an array",
            ));
        }
        let Some(mut mounts) = mounts_v.as_object() else {
            return Err(ctx.throw_type_error("`mounts` must be an object"));
        };
        let pks = mounts.get_own_property_names(
            ctx,
            zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK,
        )?;
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
                    ctx.throw_type_error(&format!("`mounts[{source}]` must be an object"))
                );
            };

            let writable_v = m.get_property_str(ctx, "writable")?;
            let writable = if writable_v.is_undefined() {
                false
            } else if writable_v.is_bool() {
                writable_v.to_bool()?
            } else {
                return Err(ctx.throw_type_error(&format!(
                    "`mounts[{source}].writable` must be a boolean"
                )));
            };

            // target — optional; same-path mount when omitted.
            let target = get_str_prop(ctx, &m, "target")?.unwrap_or_else(|| source.clone());

            let perm = if writable {
                nanosandbox::Permission::ReadWrite
            } else {
                nanosandbox::Permission::ReadOnly
            };
            b = b.mount(PathBuf::from(source), PathBuf::from(target), perm);
        }
    }

    // tmpfs: [{path, size}]
    let tmpfs_v = opts.get_property_str(ctx, "tmpfs")?;
    if !tmpfs_v.is_undefined() {
        let arr = Array::try_from(tmpfs_v)
            .map_err(|_| ctx.throw_type_error("`tmpfs` must be an array"))?;
        let len = arr.get_length_prop(ctx)? as usize;
        for i in 0..len {
            let item = arr.get_property_u32(ctx, i as u32)?;
            let Some(m) = item.as_object() else {
                return Err(ctx.throw_type_error("`tmpfs[i]` must be an object"));
            };
            let path = get_str_prop(ctx, &m, "path")?
                .ok_or_else(|| ctx.throw_type_error("`tmpfs[i].path` must be a string"))?;
            let size = get_pos_int_prop(ctx, &m, "size")?.ok_or_else(|| {
                ctx.throw_range_error("`tmpfs[i].size` must be a positive integer")
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
                .map_err(|_| ctx.throw_range_error("`maxPids` exceeds u32 range"))?,
        );
    }
    if let Some(v) = get_pos_int_prop(ctx, &opts, "maxFileSize")? {
        b = b.max_file_size(v);
    }
    if let Some(n) = get_pos_int_prop(ctx, &opts, "maxOpenFiles")? {
        b = b.max_open_files(
            u32::try_from(n)
                .map_err(|_| ctx.throw_range_error("`maxOpenFiles` exceeds u32 range"))?,
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
                        "`network` must be \"none\", \"host\" or {allow: [...]}",
                    ));
                }
            }
        } else if let Some(o) = net_v.as_object() {
            let allow_v = o.get_property_str(ctx, "allow")?;
            let allow = parse_string_array(ctx, &allow_v, "network.allow")?;
            let refs: Vec<&str> = allow.iter().map(|s| s.as_str()).collect();
            b = b.allow_network(&refs);
        } else {
            return Err(
                ctx.throw_range_error("`network` must be \"none\", \"host\" or {allow: [...]}")
            );
        }
    }

    // seccompProfile: "disabled"|"strict"|"standard"|"permissive" | string[]
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
                        "`seccompProfile` must be \"disabled\"|\"strict\"|\"standard\"|\"permissive\" or a syscall list",
                    ));
                }
            }
        } else if seccomp_v.as_object().is_some() {
            let syscalls = parse_string_array(ctx, &seccomp_v, "seccompProfile")?;
            b = b.seccomp_profile(nanosandbox::SeccompProfile::Custom(syscalls));
        } else {
            return Err(ctx.throw_range_error(
                "`seccompProfile` must be \"disabled\"|\"strict\"|\"standard\"|\"permissive\" or a syscall list",
            ));
        }
    }

    if let Some(v) = get_pos_int_prop(ctx, &opts, "uid")? {
        b = b.uid(
            u32::try_from(v).map_err(|_| ctx.throw_range_error("`uid` exceeds u32 range"))?,
        );
    }
    if let Some(v) = get_pos_int_prop(ctx, &opts, "gid")? {
        b = b.gid(
            u32::try_from(v).map_err(|_| ctx.throw_range_error("`gid` exceeds u32 range"))?,
        );
    }

    // env: Record<string, string>
    let env_v = opts.get_property_str(ctx, "env")?;
    if !env_v.is_undefined() {
        let Some(mut env_obj) = env_v.as_object() else {
            return Err(ctx.throw_type_error("`env` must be an object"));
        };
        let pks = env_obj.get_own_property_names(
            ctx,
            zuqe::JS_GPN_ENUM_ONLY | zuqe::JS_GPN_STR_OR_INT_MASK,
        )?;
        for pk in pks.iter() {
            let key_v = pk.atom.to_value(ctx)?;
            let key = key_v
                .to_string(ctx)?
                .content()
                .encode_utf8(false)
                .to_string();
            let value = env_obj
                .get_property(ctx, pk.atom)?
                .as_string(true)
                .ok_or_else(|| ctx.throw_type_error(&format!("`env.{key}` must be a string")))?
                .content()
                .encode_utf8(false)
                .to_string();
            b = b.env(key, value);
        }
    }

    if let Some(v) = get_str_prop(ctx, &opts, "hostname")? {
        b = b.hostname(v);
    }

    let sb = b
        .build()
        .map_err(|e| ctx.throw_type_error(&format!("invalid sandbox configuration: {e}")))?;

    JSSandbox::new_instance(ctx, cc.this(), SandboxPayload(Arc::new(sb))).map(JSValue::from)
}

fn payload_of_this<'a>(
    ctx: &mut ScopedContext,
    this: &'a JSValue,
) -> Result<&'a SandboxPayload, i32> {
    JSSandbox::payload_of(this).ok_or_else(|| ctx.throw_type_error("not a Sandbox instance"))
}

/// run(cmd, args?, options?) → Promise<ExecutionResult>
///
/// Execution runs on the blocking thread pool so the engine's event loop is
/// not frozen by long sandboxed commands.
fn js_sandbox_run(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let this = cc.this();
    let payload = payload_of_this(ctx, &this)?;

    // cmd — required string
    let cmd = cc
        .arg(0)
        .as_string(false)
        .ok_or_else(|| ctx.throw_type_error("`cmd` must be a string"))?
        .content()
        .encode_utf8(false)
        .to_string();

    // args — optional string[]
    let args: Vec<String> = if cc.length() > 1 && !cc.arg(1).is_undefined() {
        parse_string_array(ctx, &cc.arg(1), "args")?
    } else {
        Vec::new()
    };

    // options.stdin — optional string | Uint8Array
    let stdin: Option<Vec<u8>> = if cc.length() > 2 && !cc.arg(2).is_undefined() {
        let Some(opts) = cc.arg(2).as_object() else {
            return Err(ctx.throw_type_error("run options must be an object"));
        };
        let v = opts.get_property_str(ctx, "stdin")?;
        if v.is_undefined() {
            None
        } else if let Some(s) = v.as_string(false) {
            Some(s.content().encode_utf8(false).as_bytes().to_vec())
        } else if let Some(obj) = v.as_object()
            && let Ok(ta) = TypedArray::try_from(obj)
        {
            Some(ta.with_bytes(|b| b.to_vec()))
        } else {
            return Err(ctx.throw_type_error("`stdin` must be a string or Uint8Array"));
        }
    } else {
        None
    };

    let sb = payload.0.clone();

    let (promise, tx) = Promise::create_with_channel::<Result<nanosandbox::ExecutionResult, String>>(
        ctx,
        |cx, res| match res {
            Ok(r) => execution_result_to_js(cx, r),
            Err(msg) => Err(cx.throw_internal_error(&msg)),
        },
    )?;

    tokio::task::spawn_blocking(move || {
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let r = match stdin {
            Some(input) => sb.run_with_input(&cmd, &arg_refs, Some(&input)),
            None => sb.run(&cmd, &arg_refs),
        };
        let _ = tx.send(r.map_err(|e| format!("{e}")));
    });

    Ok(promise.into())
}

fn execution_result_to_js(ctx: &mut ScopedContext, r: nanosandbox::ExecutionResult) -> JSResult {
    fn set(o: &mut Object, ctx: &mut ScopedContext, key: &str, v: JSValue) -> Result<(), i32> {
        o.set_property_str(ctx, key, v).map(|_| ())
    }
    fn str_val(ctx: &mut ScopedContext, s: String) -> Result<JSValue, i32> {
        ctx.create_string_from_utf8(s.into_bytes())
            .map(JSValue::make_string)
    }
    fn opt_num(
        o: &mut Object,
        ctx: &mut ScopedContext,
        key: &str,
        v: Option<f64>,
    ) -> Result<(), i32> {
        match v {
            Some(n) => set(o, ctx, key, n.into()),
            None => set(o, ctx, key, JSValue::NULL),
        }
    }

    let mut o = ctx.create_base_object()?;
    let stdout = str_val(ctx, r.stdout)?;
    let stderr = str_val(ctx, r.stderr)?;
    set(&mut o, ctx, "stdout", stdout)?;
    set(&mut o, ctx, "stderr", stderr)?;
    set(&mut o, ctx, "exitCode", (r.exit_code as f64).into())?;
    set(
        &mut o,
        ctx,
        "durationMs",
        (r.duration.as_secs_f64() * 1000.0).into(),
    )?;
    set(&mut o, ctx, "killedByTimeout", r.killed_by_timeout.into())?;
    set(&mut o, ctx, "killedByOom", r.killed_by_oom.into())?;
    opt_num(&mut o, ctx, "signal", r.signal.map(|s| s as f64))?;
    opt_num(&mut o, ctx, "peakMemory", r.peak_memory.map(|m| m as f64))?;
    opt_num(
        &mut o,
        ctx,
        "cpuTimeMs",
        r.cpu_time.map(|d| d.as_secs_f64() * 1000.0),
    )?;
    Ok(o.into())
}

const SANDBOX_PROTO: &[FieldDef] = &[FieldDef::FUNC("run", 3, js_sandbox_run, 0)];

def_native_object!(
    "Sandbox",
    JSSandbox,
    SandboxPayload,
    SANDBOX_PROTO,
    Some(ClassCtorDef::new(js_sandbox_constructor, 1, true))
);

#[distributed_slice(USER_CLASS_DEFS)]
fn register_class_sandbox(rt: &mut Runtime) -> Result<ClassID, i32> {
    JSSandbox::register(rt)
}

// ---------------------------------------------------------------------------
// module init
// ---------------------------------------------------------------------------

fn js_is_supported(_ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    Ok(nanosandbox::is_platform_supported().into())
}

fn js_platform_name(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
    ctx.create_string_from_str(nanosandbox::platform_name())
        .map(JSValue::make_string)
}

const MOD_SANDBOX_ENTRIES: &[FieldDef] = &[
    FieldDef::CLASS(
        JSSandbox::CLASS_NAME,
        JSSandbox::class_id,
        zuqe::JS_PROP_ENUMERABLE,
    ),
    FieldDef::FUNC("isSupported", 0, js_is_supported, 0),
    FieldDef::FUNC("platformName", 0, js_platform_name, 0),
];

pub fn init_mod_sandbox(ctx: &mut ScopedContext) -> Result<(), i32> {
    ctx.use_classes(&[JSSandbox::class_id()])?;
    ctx.create_module("sandbox", MOD_SANDBOX_ENTRIES)?;
    Ok(())
}
