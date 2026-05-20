// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! proc.rs — runtime identity fields: argv, execArgv, env, cwd, exit

use std::path::Path;

use zuqe::{CallContext, JSResult, JSValue, ScopedContext};

pub(super) fn js_cwd(ctx: &mut ScopedContext, _cc: CallContext) -> JSResult {
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

pub(super) fn js_exit(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    use std::process::ExitCode;
    let code = if cc.length() > 0 && !cc.arg(0).is_undefined() {
        cc.arg(0).to_number(ctx).map(|n| n.to_i32()).unwrap_or(0)
    } else {
        0
    };
    ctx.rt_mut().interrupt(ExitCode::from(code as u8));
    Err(-1)
}

pub(super) fn js_env(cx: &mut ScopedContext, _cc: CallContext) -> JSResult {
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
}

/// Load key=value pairs from `.env` in the current working directory.
///
/// Supports `#` comments, blank-line skips, and single/double-quoted values.
pub(super) fn load_dotenv() -> Vec<(String, String)> {
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
