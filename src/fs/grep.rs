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

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, Sink, SinkMatch};
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

use super::{opt_str, opt_str_array, read_bool};

// Output shape of `fs.grep`, selected by `mode`.
enum GrepOutput {
    /// Default: one entry per retained line (matched, or non-matched if inverted).
    Content(Vec<Hit>),
    /// `mode:"files"` (-l): distinct files with at least one retained line.
    Files(HashSet<PathBuf>),
    /// `mode:"count"` (-c): retained-line count per file.
    Counts(Vec<(PathBuf, usize)>),
}

/// Read `p` line by line, keeping lines that do NOT match `matcher`
/// (`invert: true`, grep `-v`). Skips binary files (NUL byte). Line numbers are
/// 1-based; a trailing newline does not yield an extra empty line.
fn collect_invert_lines(
    matcher: &RegexMatcher,
    p: &Path,
    max_results: usize,
) -> std::io::Result<Vec<Hit>> {
    let data = std::fs::read(p)?;
    if data.contains(&0) {
        return Ok(Vec::new()); // binary — skip like grep-searcher
    }
    let segs: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    let trailing_nl = data.last() == Some(&b'\n');
    let n = segs.len();
    let mut hits = Vec::new();
    for (idx, raw) in segs.into_iter().enumerate() {
        // Drop the sole trailing empty segment produced by a final newline.
        if trailing_nl && idx == n - 1 && raw.is_empty() {
            continue;
        }
        let line_bytes = if raw.last() == Some(&b'\r') {
            &raw[..raw.len() - 1]
        } else {
            raw
        };
        let is_match = matcher.find(line_bytes).is_ok_and(|m| m.is_some());
        if !is_match {
            let line = String::from_utf8_lossy(line_bytes).into_owned();
            hits.push(Hit {
                file: p.to_path_buf(),
                line_number: (idx + 1) as u64,
                line,
                ranges: Vec::new(),
            });
            if hits.len() >= max_results {
                break;
            }
        }
    }
    Ok(hits)
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
    let mut mode: String = "content".to_string();
    let mut invert = false;

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
        // mode: "content" (default) | "files" (-l) | "count" (-c).
        mode = opt_str(ctx, &cc, "mode")?
            .map(|m| m.to_lowercase())
            .unwrap_or_else(|| "content".to_string());
        if mode != "files" && mode != "count" {
            mode = "content".to_string();
        }
        // invert (-v): keep lines that do NOT contain the pattern.
        invert = read_bool(ctx, &cc, "invert", false)?;
    }

    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir().map(|d| d.join(&cwd)).unwrap_or(cwd)
    };
    let cwd_rel = cwd.clone();

    // Compile the regex up front so a bad pattern surfaces as a JS error.
    // Inject (?m) (multi-line) unless the user already set mode flags, so
    // `^`/`$` anchor to line boundaries like standard grep/ripgrep (grep-searcher
    // feeds one line at a time, so without it `$` never matches).
    let compiled = if pattern.contains("(?") {
        pattern
    } else {
        format!("(?m){pattern}")
    };
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .build(&compiled)
        .map_err(|e| ctx.throw_syntax_error(&format!("invalid grep pattern: {e}")))?;

    spawn_fs_op(
        ctx,
        async move {
            tokio::task::spawn_blocking(move || {
                let mut content_hits: Vec<Hit> = Vec::new();
                let mut files_set: HashSet<PathBuf> = HashSet::new();
                let mut counts: Vec<(PathBuf, usize)> = Vec::new();

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

                    // Determine real matches for this file first.
                    // Quit at the first NUL byte so binary files (models, etc.)
                    // are not read in full — matches ripgrep's default behavior.
                    let mut searcher = Searcher::new();
                    searcher.set_binary_detection(BinaryDetection::quit(b'\x00'));
                    let mut collector = Collector {
                        matcher: &matcher,
                        file: p.clone(),
                        hits: Vec::new(),
                        max_results,
                        truncated: false,
                    };
                    if searcher.search_path(&matcher, &p, &mut collector).is_err() {
                        // Unreadable — skip silently.
                    }

                    let any_match = !collector.hits.is_empty();

                    // Retained lines depend on mode and `invert`:
                    // - files: file-level, decided below via `any_match` (-l / -L).
                    // - content/count + invert: line-level reverse (-v), since
                    //   grep-searcher's Sink only reports matched lines.
                    let file_hits: Vec<Hit> = if mode == "files" {
                        Vec::new()
                    } else if invert {
                        collect_invert_lines(&matcher, &p, max_results)
                            .map_err(|e| e.to_string())?
                    } else {
                        collector.hits
                    };

                    match mode.as_str() {
                        "files" => {
                            // -l: files with a match; -L (invert): files with none.
                            let retain = if invert { !any_match } else { any_match };
                            if retain {
                                files_set.insert(p.clone());
                            }
                        }
                        "count" => {
                            if !file_hits.is_empty() {
                                counts.push((p.clone(), file_hits.len()));
                            }
                        }
                        _ => {
                            if !file_hits.is_empty() {
                                content_hits.extend(file_hits);
                            }
                        }
                    }

                    // maxResults caps the number of returned lines (content mode).
                    if mode == "content" && content_hits.len() >= max_results {
                        break;
                    }
                }

                if mode == "content" && content_hits.len() > max_results {
                    content_hits.truncate(max_results);
                }

                let out = match mode.as_str() {
                    "files" => GrepOutput::Files(files_set),
                    "count" => GrepOutput::Counts(counts),
                    _ => GrepOutput::Content(content_hits),
                };
                Ok(out)
            })
            .await
            .map_err(|e| e.to_string())?
        },
        move |cx, out: GrepOutput| {
            // Relative-path helper honoring `absolute`.
            let rel = |f: &Path| -> String {
                if absolute {
                    f.to_string_lossy().to_string()
                } else {
                    f.strip_prefix(&cwd_rel)
                        .unwrap_or(f)
                        .to_string_lossy()
                        .to_string()
                }
            };
            // Build a JS string value first, then use it — avoids holding two
            // simultaneous mutable borrows of `cx` in a single expression.
            let mk = |cx: &mut ScopedContext, s: String| -> JSResult {
                Ok(JSValue::make_string(
                    cx.create_string_from_utf8(s.into_bytes())?,
                ))
            };
            match out {
                GrepOutput::Files(set) => {
                    let mut files: Vec<String> = set.into_iter().map(|p| rel(&p)).collect();
                    files.sort();
                    let mut vals: Vec<JSValue> = Vec::with_capacity(files.len());
                    for s in files {
                        vals.push(mk(cx, s)?);
                    }
                    Array::from_values(cx, vals).map(JSValue::from)
                }
                GrepOutput::Counts(vec) => {
                    let mut arr: Vec<JSValue> = Vec::with_capacity(vec.len());
                    for (f, c) in vec {
                        let mut obj = cx.create_base_object()?;
                        let file_val = mk(cx, rel(&f))?;
                        obj.set_property_str(cx, "file", file_val)?;
                        obj.set_property_str(cx, "count", (c as f64).into())?;
                        arr.push(obj.into());
                    }
                    Array::from_values(cx, arr).map(JSValue::from)
                }
                GrepOutput::Content(hits) => {
                    let mut arr: Vec<JSValue> = Vec::with_capacity(hits.len());
                    for h in &hits {
                        let mut obj = cx.create_base_object()?;
                        let file_val = mk(cx, rel(&h.file))?;
                        obj.set_property_str(cx, "file", file_val)?;
                        obj.set_property_str(cx, "lineNumber", (h.line_number as f64).into())?;
                        let line_val = mk(cx, h.line.clone())?;
                        obj.set_property_str(cx, "line", line_val)?;
                        // Convenience: first match offsets (empty when inverted).
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
                }
            }
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
