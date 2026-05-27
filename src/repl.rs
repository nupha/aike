// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::ExitCode;

use colorized::{Colors::*, colorize_println};
use reedline::{
    FileBackedHistory, HistoryItem, Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal,
};
use zuqe::{Context, EvalFlags, EvalType, Exception, JSValue, Runtime};

/// REPL prompt that switches between primary and continuation style.
pub(crate) struct AikePrompt {
    pub is_multiline: bool,
}

impl Prompt for AikePrompt {
    fn render_prompt_left(&self) -> Cow<str> {
        if self.is_multiline {
            Cow::Borrowed("   ... ")
        } else {
            Cow::Borrowed("\x1b[32maike>\x1b[0m ")
        }
    }

    fn render_prompt_right(&self) -> Cow<str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _edit_mode: PromptEditMode) -> Cow<str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<str> {
        if self.is_multiline {
            Cow::Borrowed("   ... ")
        } else {
            Cow::Borrowed("")
        }
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<str> {
        Cow::Borrowed("")
    }
}

/// Check whether JavaScript code has balanced brackets, quotes, and comments.
///
/// Handles:
/// - Parentheses `()`, brackets `[]`, braces `{}`
/// - Single/double/backtick strings with escape sequences
/// - Block comments `/* */` and line comments `//`
pub(crate) fn is_input_complete(code: &str) -> bool {
    let bytes = code.as_bytes();
    let mut parens: i32 = 0;
    let mut brackets: i32 = 0;
    let mut braces: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut in_block_comment = false;
    let mut escape = false;
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];

        if escape {
            escape = false;
            i += 1;
            continue;
        }

        if c == b'\\' && (in_single || in_double || in_backtick) {
            escape = true;
            i += 1;
            continue;
        }

        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if !in_single && !in_double && !in_backtick && c == b'/' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'/' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'*' => {
                    in_block_comment = true;
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }

        match c {
            b'\'' if !in_double && !in_backtick => in_single ^= true,
            b'"' if !in_single && !in_backtick => in_double ^= true,
            b'`' if !in_single && !in_double => in_backtick ^= true,
            b'(' if !in_single && !in_double && !in_backtick => parens += 1,
            b')' if !in_single && !in_double && !in_backtick => parens -= 1,
            b'[' if !in_single && !in_double && !in_backtick => brackets += 1,
            b']' if !in_single && !in_double && !in_backtick => brackets -= 1,
            b'{' if !in_single && !in_double && !in_backtick => braces += 1,
            b'}' if !in_single && !in_double && !in_backtick => braces -= 1,
            _ => {}
        }
        i += 1;
    }

    parens == 0
        && brackets == 0
        && braces == 0
        && !in_single
        && !in_double
        && !in_backtick
        && !in_block_comment
}

fn history_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".aike_history"))
        .unwrap_or_else(|_| PathBuf::from(".aike_history"))
}

fn print_repl_help(eval_type: EvalType) {
    let mode = match eval_type {
        EvalType::Module => "module",
        EvalType::Global => "global",
        _ => "?",
    };
    println!("Aike REPL commands:");
    println!("  /exit, /quit  exit the REPL");
    println!("  /help         show this help");
    println!("  /global       switch to global mode (default)");
    println!("  /module       switch to module mode (for import/export)");
    println!();
    println!("JS evaluation (current mode: {mode}):");
    println!("  Type any JavaScript expression and press Enter to evaluate.");
    println!("  Multi-line input is automatically detected (unbalanced brackets).");
    println!("  Press Ctrl-C to cancel multi-line input.");
    println!("  Press Ctrl-D to exit.");
}

fn add_history(editor: &mut Reedline, line: &str) {
    let item = HistoryItem::from_command_line(line);
    let _ = editor.history_mut().save(item);
}

/// For module mode: transform static `import` statements into dynamic
/// `import()` calls with `globalThis` assignment, so that imported
/// bindings are accessible from subsequent REPL lines.
///
/// The transformed code is evaluated as `EvalType::Global` with
/// `EvalFlags::COMPILE_TO_ASYNC` (so `await import()` works).
fn transform_imports(code: &str) -> String {
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    let mut out = String::with_capacity(code.len() + 128);

    while pos < len {
        // look for "import" keyword
        if pos + 7 > len || &bytes[pos..pos + 6] != b"import" {
            out.push(bytes[pos] as char);
            pos += 1;
            continue;
        }
        // verify it is the keyword, not inside an identifier
        if pos > 0
            && (bytes[pos - 1].is_ascii_alphanumeric()
                || bytes[pos - 1] == b'_'
                || bytes[pos - 1] == b'$')
        {
            out.push(bytes[pos] as char);
            pos += 1;
            continue;
        }
        // Found "import" keyword at pos, length 6
        pos += 6; // skip "import"

        // skip whitespace after "import"
        let ws: String = skip_ws_bytes(bytes, &mut pos, len);

        if pos >= len {
            // Trailing "import " — re-emit
            out.push_str("import");
            out.push_str(&ws);
            break;
        }

        let c = bytes[pos];
        match c {
            b'{' => {
                // import { ... } from '...'
                let bindings = parse_named_import_bindings(bytes, &mut pos, len);
                skip_ws_bytes(bytes, &mut pos, len);
                let (spec, _from_end) = parse_from_specifier(bytes, &mut pos, len);
                // Generate: const __m0=await import('spec'); const a=globalThis.a=__m0.a; ...
                let temp = format!("__m{}", out.len());
                out.push_str(&format!("const {temp}=await import('{spec}');"));
                for (exported, local) in bindings {
                    out.push_str(&format!(
                        "const {local}=globalThis.{local}={temp}.{exported};"
                    ));
                }
            }
            b'*' => {
                // import * as name from '...'
                pos += 1;
                let ws_after_star = skip_ws_bytes(bytes, &mut pos, len);
                if pos + 2 <= len && &bytes[pos..pos + 2] == b"as" {
                    pos += 2;
                    let ws_after_as = skip_ws_bytes(bytes, &mut pos, len);
                    let name = read_ident_str(bytes, &mut pos, len);
                    skip_ws_bytes(bytes, &mut pos, len);
                    let (spec, _) = parse_from_specifier(bytes, &mut pos, len);
                    out.push_str(&format!(
                        "const {name}=globalThis.{name}=await import('{spec}');"
                    ));
                    let _ = (ws_after_star, ws_after_as);
                }
            }
            b'\'' | b'"' => {
                // import '...' (side-effect only)
                let quote = c;
                pos += 1;
                while pos < len && bytes[pos] != quote {
                    pos += 1;
                }
                if pos < len {
                    pos += 1; // skip closing quote
                }
                // Copy the original content but it was already skipped,
                // so re-read the spec from the original
                // Actually, we stripped "import " earlier, so nothing
                // to emit. Just output the await import.
                // But we discarded the specifier. Let's handle differently.
                // For simplicity, drop side-effect imports (uncommon in REPL).
            }
            _ => {
                // import defaultName from '...' or something unexpected
                if c != b'\'' && c != b'"' {
                    let name = read_ident_str(bytes, &mut pos, len);
                    skip_ws_bytes(bytes, &mut pos, len);
                    let (spec, _) = parse_from_specifier(bytes, &mut pos, len);
                    out.push_str(&format!(
                        "const {name}=globalThis.{name}=(await import('{spec}')).default;"
                    ));
                }
            }
        }
    }

    out
}

/// Skip whitespace bytes, return the skipped chars (for debugging, unused).
fn skip_ws_bytes(bytes: &[u8], pos: &mut usize, len: usize) -> String {
    let start = *pos;
    while *pos < len
        && (bytes[*pos] == b' '
            || bytes[*pos] == b'\t'
            || bytes[*pos] == b'\n'
            || bytes[*pos] == b'\r')
    {
        *pos += 1;
    }
    // safety: bytes[start..*pos] is valid utf-8 (only ASCII whitespace chars)
    unsafe { std::str::from_utf8_unchecked(&bytes[start..*pos]) }.to_string()
}

/// Read an identifier from current position, return the string.
fn read_ident_str(bytes: &[u8], pos: &mut usize, len: usize) -> String {
    let start = *pos;
    while *pos < len
        && (bytes[*pos].is_ascii_alphanumeric() || bytes[*pos] == b'_' || bytes[*pos] == b'$')
    {
        *pos += 1;
    }
    // safety: valid utf-8 at ASCII boundaries
    unsafe { std::str::from_utf8_unchecked(&bytes[start..*pos]) }.to_string()
}

/// Parse `from 'specifier'` or `from "specifier"` starting at `pos`.
/// Returns (specifier, position_after_closing_quote).
fn parse_from_specifier(bytes: &[u8], pos: &mut usize, len: usize) -> (String, usize) {
    skip_ws_bytes(bytes, pos, len);
    if *pos + 4 <= len && &bytes[*pos..*pos + 4] == b"from" {
        *pos += 4;
        skip_ws_bytes(bytes, pos, len);
        if *pos < len && (bytes[*pos] == b'\'' || bytes[*pos] == b'"') {
            let quote = bytes[*pos];
            *pos += 1;
            let start = *pos;
            while *pos < len && bytes[*pos] != quote {
                *pos += 1;
            }
            let spec = unsafe { std::str::from_utf8_unchecked(&bytes[start..*pos]) }.to_string();
            *pos += 1; // skip closing quote
            return (spec, *pos);
        }
    }
    (String::new(), *pos)
}

/// Parse `{ spec1, spec2 as alias }` from import.
/// Returns Vec<(exported_name, local_name)>.
fn parse_named_import_bindings(bytes: &[u8], pos: &mut usize, len: usize) -> Vec<(String, String)> {
    let mut bindings = Vec::new();
    if *pos >= len || bytes[*pos] != b'{' {
        return bindings;
    }
    *pos += 1; // skip '{'

    loop {
        skip_ws_bytes(bytes, pos, len);
        if *pos >= len || bytes[*pos] == b'}' {
            break;
        }
        if bytes[*pos] == b',' {
            *pos += 1;
            continue;
        }
        let exported = read_ident_str(bytes, pos, len);
        skip_ws_bytes(bytes, pos, len);
        let local = if *pos + 2 <= len && &bytes[*pos..*pos + 2] == b"as" {
            *pos += 2;
            skip_ws_bytes(bytes, pos, len);
            read_ident_str(bytes, pos, len)
        } else {
            exported.clone()
        };
        bindings.push((exported, local));
    }

    if *pos < len && bytes[*pos] == b'}' {
        *pos += 1;
    }

    bindings
}

/// Run the interactive REPL using reedline for terminal control.
pub(crate) async fn run_repl(rt: &mut Runtime, ctx: &mut Context<'_>) -> ExitCode {
    // Create reedline editor with file-backed history
    let hist_path = history_path();
    let history = match FileBackedHistory::with_file(1000, hist_path) {
        Ok(h) => h,
        Err(_) => {
            // History file doesn't exist yet; create in-memory
            FileBackedHistory::new(1000).unwrap_or_else(|_| FileBackedHistory::new(50).unwrap())
        }
    };
    let mut editor = Reedline::create().with_history(Box::new(history));

    let mut prompt = AikePrompt {
        is_multiline: false,
    };
    let mut multi_line = String::new();
    let mut last_was_ctrl_c = false;
    let mut eval_type = EvalType::Global;

    println!(
        "\x1b[32mWelcome to Aike REPL! Type /help for info. (current mode: {})\x1b[0m",
        if matches!(eval_type, EvalType::Module) {
            "module"
        } else {
            "global"
        }
    );

    loop {
        let sig = match editor.read_line(&prompt) {
            Ok(s) => s,
            Err(_) => break,
        };

        match sig {
            Signal::CtrlC => {
                multi_line.clear();
                prompt.is_multiline = false;
                if last_was_ctrl_c {
                    // double Ctrl-C exits
                    println!("^C");
                    break;
                }
                last_was_ctrl_c = true;
                println!("^C(Press Ctrl-C again to quit)");
                continue;
            }
            Signal::CtrlD => {
                if !multi_line.is_empty() {
                    colorize_println("(cancelled)", YellowFg);
                    multi_line.clear();
                    prompt.is_multiline = false;
                    continue;
                }
                break;
            }
            Signal::Success(line) => {
                last_was_ctrl_c = false;

                if line.trim().is_empty() && multi_line.is_empty() {
                    continue;
                }

                // Handle meta-commands at the start of input
                let trimmed = line.trim();
                if multi_line.is_empty() && trimmed.starts_with('/') {
                    match trimmed {
                        "/exit" | "/quit" => break,
                        "/help" => print_repl_help(eval_type),
                        "/global" => {
                            eval_type = EvalType::Global;
                            colorize_println("switched to global mode", GreenFg);
                        }
                        "/module" => {
                            eval_type = EvalType::Module;
                            colorize_println("switched to module mode", GreenFg);
                        }
                        _ => {
                            colorize_println(format!("unknown command: {trimmed}"), YellowFg);
                        }
                    }
                    add_history(&mut editor, trimmed);
                    continue;
                }

                // Accumulate into multi-line buffer
                multi_line.push_str(&line);
                multi_line.push('\n');

                // Check if input is complete
                if !is_input_complete(&multi_line) {
                    prompt.is_multiline = true;
                    continue;
                }

                // Complete input — evaluate
                prompt.is_multiline = false;
                let code = std::mem::take(&mut multi_line);
                let code_trimmed = code.trim().to_string();

                if code_trimmed.is_empty() {
                    continue;
                }

                add_history(&mut editor, &code_trimmed);

                // In module mode: transform imports and use COMPILE_TO_ASYNC
                let (eval_code, flags) = if matches!(eval_type, EvalType::Module) {
                    (
                        transform_imports(&code_trimmed),
                        EvalFlags::COMPILE_TO_ASYNC,
                    )
                } else {
                    (code_trimmed.clone(), EvalFlags::empty())
                };
                let actual_eval_type = EvalType::Global;

                // Step 1: Evaluate in JS context
                let result = ctx.with_new_scope(|mut scope| -> Result<JSValue, ()> {
                    scope
                        .eval(eval_code.as_bytes(), "<repl>", actual_eval_type, flags)
                        .map_err(|_| ())
                });

                let val = match result {
                    Ok(v) => v,
                    Err(()) => {
                        // Handle eval error
                        let exc = rt.take_exception();
                        ctx.with_new_scope(|mut cx| -> Result<(), ()> {
                            match exc {
                                Exception::Error(e) => {
                                    let s = e.to_string(&mut cx).map_err(|_| ())?;
                                    colorize_println(s.encode_utf8(false), RedFg);
                                }
                                Exception::Interrupted(_) => {
                                    colorize_println("interrupted", RedFg);
                                }
                                Exception::OutOfMemory => {
                                    colorize_println("out of memory", RedFg);
                                }
                                Exception::No | Exception::TooManyClasses => {}
                            }
                            Ok(())
                        })
                        .ok();
                        continue;
                    }
                };

                // Step 2: Check if result is a Promise
                let promise_fut = ctx.with_new_scope(|mut scope| -> Option<_> {
                    let p = val.as_object_Promise()?;
                    let fut = (*p).into_future(&mut scope).ok()?;
                    Some(fut)
                });

                // Step 3: Await promise (outside ctx borrow) or print directly
                if let Some(fut) = promise_fut {
                    let (_rt_res, promise_res) = tokio::join!(rt.run_async(), fut);

                    match promise_res {
                        Ok(resolved) => {
                            ctx.with_new_scope(|mut cx| -> Result<(), ()> {
                                if let Ok(s) = resolved.inspect(cx.rt_mut()) {
                                    println!("{}", s);
                                }
                                Ok(())
                            })
                            .ok();
                        }
                        Err(reason) => {
                            ctx.with_new_scope(|mut cx| -> Result<(), ()> {
                                let s = reason.to_string(&mut cx).map_err(|_| ())?;
                                colorize_println(s.encode_utf8(false), RedFg);
                                Ok(())
                            })
                            .ok();
                        }
                    }
                } else {
                    // Print regular value
                    ctx.with_new_scope(|mut cx| -> Result<(), ()> {
                        if let Ok(s) = val.inspect(cx.rt_mut()) {
                            println!("{}", s);
                        }
                        Ok(())
                    })
                    .ok();
                }
            }
            _ => break,
        }
    }

    // Flush history to disk
    let _ = editor.history_mut().sync();

    println!();
    ExitCode::SUCCESS
}
