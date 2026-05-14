// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::path::Path;

use zuqe::{JS_PROP_C_W_E, Object, ScopedContext};

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

/// Build `process` object exposing system info.
pub fn build_process_object(ctx: &mut ScopedContext) -> Result<Object, i32> {
    let mut process = ctx.create_base_object()?;

    let mut env_obj = ctx.create_base_object()?;

    // 1) Load .env defaults (lower priority)
    for (key, value) in load_dotenv() {
        let v = ctx.create_string_from_utf8(value.as_bytes().to_vec())?;
        env_obj.set_property_str(ctx, &key, v.into())?;
    }

    // 2) Overlay real system env vars (higher priority), matching Bun behaviour
    for (key, value) in std::env::vars() {
        let v = ctx.create_string_from_utf8(value.as_bytes().to_vec())?;
        env_obj.set_property_str(ctx, &key, v.into())?;
    }

    process.define_property_value_str(ctx, "env", env_obj.into(), JS_PROP_C_W_E)?;

    Ok(process)
}
