// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use zuqe::{Array, CallContext, JSResult, JSValue, ScopedContext};

use super::file::spawn_fs_op;

/// Collect all paths matched by `pattern`, resolved relative to `base` unless
/// the pattern is absolute. Applies the given match options.
fn glob_abs(
    base: &Path,
    pattern: &str,
    case_sensitive: bool,
    dot: bool,
) -> Result<Vec<PathBuf>, String> {
    let pat = if Path::new(pattern).is_absolute() {
        PathBuf::from(pattern)
    } else {
        base.join(pattern)
    };
    let options = glob::MatchOptions {
        case_sensitive,
        require_literal_separator: true,
        require_literal_leading_dot: !dot,
    };
    let paths = glob::glob_with(&pat.to_string_lossy(), options).map_err(|e| e.to_string())?;
    Ok(paths.filter_map(Result::ok).collect())
}

use super::{opt_str, opt_str_array, read_bool};

// ---------------------------------------------------------------------------
// glob - match files against a glob pattern
//   glob(pattern[, { cwd, exclude, dot, caseSensitive, absolute }])
// ---------------------------------------------------------------------------
pub(super) fn js_fs_glob(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pattern = cc.arg(0).to_string(ctx)?;
    let pattern = pattern.content().encode_utf8(false).to_string();

    // Parse options (only when argument 1 is an object).
    let mut cwd: Option<PathBuf> = None;
    let mut excludes: Vec<String> = Vec::new();
    let mut dot = false;
    let mut case_sensitive = true;
    let mut absolute = false;
    if cc.length() > 1 && cc.arg(1).is_object() {
        cwd = opt_str(ctx, &cc, "cwd")?.map(PathBuf::from);
        excludes = opt_str_array(ctx, &cc, "exclude")?;
        dot = read_bool(ctx, &cc, "dot", false)?;
        case_sensitive = read_bool(ctx, &cc, "caseSensitive", true)?;
        absolute = read_bool(ctx, &cc, "absolute", false)?;
    }

    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    // Resolve a relative cwd against the process cwd so results are comparable
    // and `absolute: true` yields truly absolute paths.
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir().map(|d| d.join(&cwd)).unwrap_or(cwd)
    };
    // Owned copy for the converter closure (the future moves the original).
    let cwd_rel = cwd.clone();

    spawn_fs_op(
        ctx,
        async move {
            // glob walks the filesystem lazily, so run it on the blocking pool.
            tokio::task::spawn_blocking(move || {
                let mut matches = glob_abs(&cwd, &pattern, case_sensitive, dot)?;
                // Exclude: collect the absolute paths of every exclude pattern
                // and drop them from the result.
                if !excludes.is_empty() {
                    let mut blocked: HashSet<PathBuf> = HashSet::new();
                    for ex in &excludes {
                        for p in glob_abs(&cwd, ex, case_sensitive, dot)? {
                            blocked.insert(p);
                        }
                    }
                    matches.retain(|p| !blocked.contains(p));
                }
                Ok(matches)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        move |cx, matches: Vec<PathBuf>| {
            let values = matches
                .into_iter()
                .map(|p| {
                    // Return paths relative to cwd unless `absolute`.
                    let out = if absolute {
                        p
                    } else {
                        p.strip_prefix(&cwd_rel).unwrap_or(&p).to_path_buf()
                    };
                    out.to_string_lossy().to_string()
                })
                .collect::<Vec<_>>();
            let mut arr = Vec::<JSValue>::with_capacity(values.len());
            for s in &values {
                let js = cx.create_string_from_utf8(s.as_bytes().to_vec())?;
                arr.push(JSValue::make_string(js));
            }
            Array::from_values(cx, arr).map(JSValue::from)
        },
    )
    .map(JSValue::from)
}
