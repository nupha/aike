// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! `fs.grep` — ripgrep-style file content search.
//!
//! Uses grep-matcher (matcher abstraction) + grep-regex (regex backend) +
//! grep-searcher (line-oriented file reader) for the matching, and the
//! `ignore` crate for `.gitignore`/`.ignore`/hidden-file aware traversal.
//!
//!   grep(pattern[, { cwd, paths, include, exclude, caseSensitive, dot,
//!                    respectIgnore, maxResults, absolute }])
//!
//! Returns an array of match objects:
//!   { file, lineNumber, line, start, end, ranges: [{start,end}] }

use std::path::{Path, PathBuf};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, Sink, SinkMatch};
use ignore::WalkBuilder;
use zuqe::{Array, CallContext, JSResult, JSValue, ScopedContext};

use super::file::spawn_fs_op;

/// A single grep hit (one matching line), with all match ranges on it.
struct Hit {
    file: PathBuf,
    line_number: u64,
    line: String,
    /// Byte offsets of every match within `line`.
    ranges: Vec<(usize, usize)>,
}

/// Sink that records every line matching the pattern.
struct Collector<'a> {
    matcher: &'a RegexMatcher,
    file: PathBuf,
    hits: Vec<Hit>,
    max_results: usize,
    truncated: bool,
}

impl<'a> Sink for Collector<'a> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        if self.truncated {
            return Ok(false);
        }
        let bytes = mat.bytes();
        // Enumerate every match on this line via the matcher.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let _ = self.matcher.find_iter(bytes, |m| {
            ranges.push((m.start(), m.end()));
            true // keep iterating over all matches on the line
        });
        if ranges.is_empty() {
            return Ok(true);
        }
        let line = String::from_utf8_lossy(bytes).into_owned();
        self.hits.push(Hit {
            file: self.file.clone(),
            line_number: mat.line_number().unwrap_or(0),
            line,
            ranges,
        });
        if self.hits.len() >= self.max_results {
            self.truncated = true;
            return Ok(false);
        }
        Ok(true)
    }
}

/// Read an optional boolean option, falling back to `default`.
fn read_bool(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    name: &str,
    default: bool,
) -> Result<bool, i32> {
    let v = cc.arg(1).get_property_str(ctx, name)?;
    if v.is_undefined() {
        Ok(default)
    } else {
        v.to_bool()
    }
}

/// Read an optional string option.
fn opt_str(ctx: &mut ScopedContext, cc: &CallContext, name: &str) -> Result<Option<String>, i32> {
    let v = cc.arg(1).get_property_str(ctx, name)?;
    if v.is_undefined() {
        Ok(None)
    } else {
        let s = v.to_string(ctx)?;
        Ok(Some(s.content().encode_utf8(false).to_string()))
    }
}

/// Read an optional string-array option.
fn opt_str_array(
    ctx: &mut ScopedContext,
    cc: &CallContext,
    name: &str,
) -> Result<Vec<String>, i32> {
    let v = cc.arg(1).get_property_str(ctx, name)?;
    let mut out = Vec::new();
    if let Some(arr) = v.as_object_Array() {
        for item in arr.payload().values() {
            if let Some(s) = item.as_string(true) {
                out.push(s.content().encode_utf8(false).to_string());
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// grep - ripgrep-style content search over a directory tree
// ---------------------------------------------------------------------------
pub(super) fn js_fs_grep(ctx: &mut ScopedContext, cc: CallContext) -> JSResult {
    let pattern = cc.arg(0).to_string(ctx)?;
    let pattern = pattern.content().encode_utf8(false).to_string();

    let mut cwd: Option<PathBuf> = None;
    let mut paths: Vec<String> = vec!["**/*".to_string()];
    let mut include: Vec<String> = Vec::new();
    let mut exclude: Vec<String> = Vec::new();
    let mut case_sensitive = false;
    let mut dot = false;
    let mut respect_ignore = true;
    let mut absolute = false;
    let mut max_results = 1000usize;
    if cc.length() > 1 && cc.arg(1).is_object() {
        cwd = opt_str(ctx, &cc, "cwd")?.map(PathBuf::from);
        include = opt_str_array(ctx, &cc, "include")?;
        exclude = opt_str_array(ctx, &cc, "exclude")?;
        if let Some(p) = opt_str(ctx, &cc, "paths")? {
            let parsed: Vec<String> = p
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !parsed.is_empty() {
                paths = parsed;
            }
        }
        case_sensitive = read_bool(ctx, &cc, "caseSensitive", false)?;
        dot = read_bool(ctx, &cc, "dot", false)?;
        respect_ignore = read_bool(ctx, &cc, "respectIgnore", true)?;
        absolute = read_bool(ctx, &cc, "absolute", false)?;
        if let Some(n) = opt_str(ctx, &cc, "maxResults")? {
            if let Ok(n) = n.parse::<usize>() {
                max_results = n.max(1);
            }
        }
    }

    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir().map(|d| d.join(&cwd)).unwrap_or(cwd)
    };
    let cwd_rel = cwd.clone();

    // Compile the regex up front so a bad pattern surfaces as a JS error.
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .build(&pattern)
        .map_err(|e| ctx.throw_syntax_error(&format!("invalid grep pattern: {e}")))?;

    spawn_fs_op(
        ctx,
        async move {
            tokio::task::spawn_blocking(move || {
                let mut all_hits: Vec<Hit> = Vec::new();
                let mut truncated = false;

                // Walk the cwd directory so the `ignore` crate can apply
                // .gitignore/.ignore/hidden rules natively during traversal.
                // (Walking individual glob-expanded files would skip those rules.)
                let mut builder = WalkBuilder::new(&cwd);
                builder
                    .hidden(!dot)
                    .require_git(false)
                    .git_ignore(respect_ignore)
                    .git_global(respect_ignore)
                    .git_exclude(respect_ignore)
                    .ignore(respect_ignore)
                    .ignore_case_insensitive(!case_sensitive);
                // If the user gave explicit `paths` globs, also add them as
                // additional walk roots (relative to cwd).
                for path_glob in &paths {
                    if path_glob == "**/*" {
                        continue; // already covered by walking cwd
                    }
                    let root = if Path::new(path_glob).is_absolute() {
                        PathBuf::from(path_glob)
                    } else {
                        cwd.join(path_glob)
                    };
                    builder.add(&root);
                }

                for entry in builder.build() {
                    if truncated {
                        break;
                    }
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    let ftype = match entry.file_type() {
                        Some(ft) => ft,
                        None => continue,
                    };
                    if !ftype.is_file() {
                        continue;
                    }
                    let p = entry.path().to_path_buf();
                    // Restrict to the user `paths` globs (relative match).
                    if !paths.iter().any(|g| path_matches(&p, g, dot, &cwd)) {
                        continue;
                    }
                    // Apply user include/exclude globs on the path.
                    if !include.is_empty()
                        && !include.iter().any(|g| path_matches(&p, g, dot, &cwd))
                    {
                        continue;
                    }
                    if exclude.iter().any(|g| path_matches(&p, g, dot, &cwd)) {
                        continue;
                    }

                    let mut searcher = Searcher::new();
                    let mut collector = Collector {
                        matcher: &matcher,
                        file: p.clone(),
                        hits: Vec::new(),
                        max_results,
                        truncated: false,
                    };
                    // grep-searcher skips binary files automatically.
                    if searcher.search_path(&matcher, &p, &mut collector).is_err() {
                        // Unreadable / binary — skip silently.
                    }
                    if !collector.hits.is_empty() {
                        all_hits.extend(collector.hits);
                    }
                    if all_hits.len() >= max_results {
                        truncated = true;
                        break;
                    }
                }

                // Cap results to maxResults (a single file may exceed it).
                if all_hits.len() > max_results {
                    all_hits.truncate(max_results);
                    truncated = true;
                }
                Ok((all_hits, truncated))
            })
            .await
            .map_err(|e| e.to_string())?
        },
        move |cx, (hits, _truncated): (Vec<Hit>, bool)| {
            let mut arr: Vec<JSValue> = Vec::with_capacity(hits.len());
            for h in &hits {
                let mut obj = cx.create_base_object()?;
                let file = if absolute {
                    h.file.to_string_lossy().to_string()
                } else {
                    h.file
                        .strip_prefix(&cwd_rel)
                        .unwrap_or(&h.file)
                        .to_string_lossy()
                        .to_string()
                };
                let file_val = JSValue::make_string(cx.create_string_from_utf8(file.into_bytes())?);
                obj.set_property_str(cx, "file", file_val)?;
                obj.set_property_str(cx, "lineNumber", (h.line_number as f64).into())?;
                let line_val =
                    JSValue::make_string(cx.create_string_from_utf8(h.line.clone().into_bytes())?);
                obj.set_property_str(cx, "line", line_val)?;
                // Convenience: first match offsets.
                let (start, end) = h.ranges.first().copied().unwrap_or((0, 0));
                obj.set_property_str(cx, "start", (start as f64).into())?;
                obj.set_property_str(cx, "end", (end as f64).into())?;
                let mut ranges_arr: Vec<JSValue> = Vec::with_capacity(h.ranges.len());
                for (s, e) in &h.ranges {
                    let mut r = cx.create_base_object()?;
                    r.set_property_str(cx, "start", (*s as f64).into())?;
                    r.set_property_str(cx, "end", (*e as f64).into())?;
                    ranges_arr.push(r.into());
                }
                let ranges_val = Array::from_values(cx, ranges_arr)?;
                obj.set_property_str(cx, "ranges", ranges_val.into())?;
                arr.push(obj.into());
            }
            Array::from_values(cx, arr).map(JSValue::from)
        },
    )
    .map(JSValue::from)
}

/// Match a path against a glob `pattern` (supports `**`, `*`, `?`).
/// The pattern is matched against the path relative to `cwd` first (so
/// `*.txt`, `src/*.rs` work), then against the basename, then the absolute
/// path. Returns true when any matches.
fn path_matches(path: &Path, pattern: &str, dot: bool, cwd: &Path) -> bool {
    let opts = glob::MatchOptions {
        case_sensitive: cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: !dot,
    };
    let rel = path.strip_prefix(cwd).unwrap_or(path);
    let rel_str = rel.to_string_lossy();
    let has_wild = pattern.contains('*') || pattern.contains('?');

    // 1) Relative-to-cwd glob (e.g. "*.txt", "src/*.rs", "**/*.rs").
    if has_wild {
        if glob::Pattern::new(pattern)
            .map(|p| p.matches(&rel_str))
            .unwrap_or(false)
        {
            return true;
        }
        // Absolute path glob (e.g. "/abs/**/*.rs") — only when the pattern
        // itself is absolute, to avoid matching a literal absolute path as a
        // self-glob (which would make every file match).
        if Path::new(pattern).is_absolute() {
            if glob::glob_with(pattern, opts)
                .map(|mut it| {
                    it.any(|p| match p {
                        Ok(p) => p == path,
                        Err(_) => false,
                    })
                })
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    // 2) Basename match.
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy();
        if has_wild {
            if glob::Pattern::new(pattern)
                .map(|p| p.matches(&name))
                .unwrap_or(false)
            {
                return true;
            }
        } else if name == pattern {
            return true;
        }
    }
    false
}
