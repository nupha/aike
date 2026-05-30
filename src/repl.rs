// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::ExitCode;

use colorized::{Colors::*, colorize_println};
use nu_ansi_term::Style;
use reedline::{
    FileBackedHistory, Highlighter, HistoryItem, Prompt, PromptEditMode, PromptHistorySearch,
    Reedline, Signal, StyledText,
};
use zuqe::{
    EvalFlags, EvalType, Exception, Runtime, TopLevelContext, builtins::promise::PromiseState,
};

// ---------------------------------------------------------------------------
// Styles — computed once with LazyLock (Rust 1.80+)
//
// Notes on color choices:
//   - The user's terminal does not render bold, so we rely on distinct ANSI
//     base colours rather than bold/bright variants.
//   - Light* variants are used where a bright colour is needed for contrast.
// ---------------------------------------------------------------------------

/// Plain terminal default (white/light on dark backgrounds).
static STYLE_DEFAULT: std::sync::LazyLock<Style> = std::sync::LazyLock::new(Style::new);

/// Dark gray — clearly dimmer than default text.
static STYLE_COMMENT: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().italic().fg(nu_ansi_term::Color::DarkGray));

/// Bold cyan — kept as-is (user confirmed this colour works).
static STYLE_STRING: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().bold().fg(nu_ansi_term::Color::Cyan));

/// Cyan + italic — distinguish regex from plain string (terminal can't render bold).
static STYLE_REGEX: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().italic().fg(nu_ansi_term::Color::Cyan));

/// Yellow — clearly different from everything else.
static STYLE_NUMBER: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::Yellow));

/// Bright blue — distinct from default, comments, strings, numbers.
static STYLE_KEYWORD: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().bold().fg(nu_ansi_term::Color::LightBlue));

/// Bright green — distinct from blue keywords and default text.
static STYLE_FUNCTION: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::LightGreen));

/// Magenta — uncommon colour that stands out.
static STYLE_TYPE: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::Magenta));

/// Terminal default — same as STYLE_DEFAULT (no colour).
static STYLE_IDENTIFIER: std::sync::LazyLock<Style> = std::sync::LazyLock::new(Style::new);

/// Red — obvious error indicator.
static STYLE_ERROR: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::Red));

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// REPL prompt that also provides syntax highlighting.
pub(crate) struct AikePrompt {
    pub is_multiline: bool,
}

impl Prompt for AikePrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        if self.is_multiline {
            Cow::Borrowed("   ... ")
        } else {
            Cow::Borrowed("\x1b[32maike>\x1b[0m ")
        }
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _edit_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        if self.is_multiline {
            Cow::Borrowed("   ... ")
        } else {
            Cow::Borrowed("")
        }
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}

impl Highlighter for AikePrompt {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        colorize_js(line)
    }
}

// ---------------------------------------------------------------------------
// Syntax colouring — ported from repl.js colorize_js()
// ---------------------------------------------------------------------------

fn colorize_js(code: &str) -> StyledText {
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut styled = StyledText::new();
    let mut seg_start: usize = 0;
    let mut cur_style: &Style = &STYLE_DEFAULT;
    let mut i = 0;

    // state tracking
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut in_block_comment = false;
    let mut can_regex = true;

    /// Flush accumulated text (seg_start..i) with the *current* style,
    /// then advance seg_start to i without changing cur_style.
    macro_rules! flush {
        () => {{
            if i > seg_start {
                styled.push((cur_style.clone(), code[seg_start..i].to_string()));
            }
            seg_start = i;
        }};
    }

    /// Emit a single token in a *new* style (style must be &Style).
    macro_rules! emit_token {
        ($st:expr, $token:expr) => {{
            if seg_start < i {
                styled.push((cur_style.clone(), code[seg_start..i].to_string()));
            }
            styled.push(($st.clone(), $token.to_string()));
            seg_start = i;
        }};
    }

    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

    while i < len {
        let c = bytes[i];

        // --- string contexts ------------------------------------------------
        if in_single {
            if c == b'\n' {
                flush!();
                cur_style = &STYLE_ERROR;
                i += 1;
                flush!();
                cur_style = &STYLE_DEFAULT;
                in_single = false;
                continue;
            }
            if c == b'\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if c == b'\'' {
                i += 1;
                flush!();
                cur_style = &STYLE_DEFAULT;
                in_single = false;
                continue;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'\n' {
                flush!();
                cur_style = &STYLE_ERROR;
                i += 1;
                flush!();
                cur_style = &STYLE_DEFAULT;
                in_double = false;
                continue;
            }
            if c == b'\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if c == b'"' {
                i += 1;
                flush!();
                cur_style = &STYLE_DEFAULT;
                in_double = false;
                continue;
            }
            i += 1;
            continue;
        }
        if in_backtick {
            if c == b'\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if c == b'`' {
                i += 1;
                flush!();
                cur_style = &STYLE_DEFAULT;
                in_backtick = false;
                continue;
            }
            i += 1;
            continue;
        }

        // --- block comment --------------------------------------------------
        if in_block_comment {
            if c == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                let end = i + 2;
                if end > seg_start {
                    styled.push((STYLE_COMMENT.clone(), code[seg_start..end].to_string()));
                }
                seg_start = end;
                i = end;
                cur_style = &STYLE_DEFAULT;
                in_block_comment = false;
                continue;
            }
            i += 1;
            continue;
        }

        match c {
            // whitespace — just advance
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,

            // --- / operator, /comment, or /regex ---------------------------
            b'/' => {
                if i + 1 < len {
                    match bytes[i + 1] {
                        b'*' => {
                            // block comment start: flush previous, emit /* in COMMENT
                            flush!();
                            cur_style = &STYLE_COMMENT;
                            i += 2;
                            in_block_comment = true;
                        }
                        b'/' => {
                            // line comment: flush, then emit whole comment in COMMENT
                            flush!();
                            cur_style = &STYLE_COMMENT;
                            i += 2;
                            while i < len && bytes[i] != b'\n' {
                                i += 1;
                            }
                            flush!();
                            cur_style = &STYLE_DEFAULT;
                        }
                        _ if can_regex => {
                            // regex literal
                            flush!();
                            cur_style = &STYLE_REGEX;
                            i += 1;
                            let mut in_class = false;
                            while i < len {
                                let rc = bytes[i];
                                if rc == b'\n' {
                                    flush!();
                                    cur_style = &STYLE_ERROR;
                                    break;
                                }
                                if rc == b'\\' && i + 1 < len {
                                    i += 2;
                                    continue;
                                }
                                if in_class {
                                    if rc == b']' {
                                        in_class = false;
                                    }
                                    i += 1;
                                    continue;
                                }
                                if rc == b'[' {
                                    in_class = true;
                                    i += 1;
                                    continue;
                                }
                                if rc == b'/' {
                                    i += 1;
                                    while i < len && is_word(bytes[i]) {
                                        i += 1;
                                    }
                                    break;
                                }
                                i += 1;
                            }
                            flush!();
                            cur_style = &STYLE_DEFAULT;
                        }
                        _ => {
                            i += 1;
                        }
                    }
                } else {
                    i += 1;
                }
            }

            // --- strings ---------------------------------------------------
            // Opening quote: flush preceding, emit quote in STRING, switch cur_style
            b'\'' => {
                emit_token!(&*STYLE_STRING, &code[i..i + 1]);
                i += 1;
                seg_start = i;
                cur_style = &STYLE_STRING;
                in_single = true;
            }
            b'"' => {
                emit_token!(&*STYLE_STRING, &code[i..i + 1]);
                i += 1;
                seg_start = i;
                cur_style = &STYLE_STRING;
                in_double = true;
            }
            b'`' => {
                emit_token!(&*STYLE_STRING, &code[i..i + 1]);
                i += 1;
                seg_start = i;
                cur_style = &STYLE_STRING;
                in_backtick = true;
            }

            // ++ / --
            b'+' | b'-' => {
                if i + 1 < len && bytes[i + 1] == c {
                    i += 2;
                } else {
                    i += 1;
                }
                can_regex = true;
            }

            b'(' | b'[' | b'{' => {
                flush!();
                can_regex = true;
                i += 1;
            }
            b')' | b']' | b'}' => {
                flush!();
                can_regex = false;
                i += 1;
            }

            // --- numbers ---------------------------------------------------
            b'0'..=b'9' => {
                flush!();
                cur_style = &STYLE_NUMBER;
                while i < len
                    && (is_word(bytes[i])
                        || (bytes[i] == b'.' && (i + 1 >= len || bytes[i + 1] != b'.')))
                {
                    i += 1;
                }
                flush!();
                cur_style = &STYLE_DEFAULT;
            }

            // --- identifiers / keywords ------------------------------------
            _ if is_word(c) => {
                let id_start = i;
                // flush text before the word with current (default) style
                flush!();
                while i < len && is_word(bytes[i]) {
                    i += 1;
                }
                let word = &code[id_start..i];

                // Default: most identifiers (keywords, function calls, types)
                // allow regex to follow — per original repl.js parse_identifier().
                can_regex = true;

                if is_js_keyword(word) {
                    styled.push((STYLE_KEYWORD.clone(), word.to_string()));
                    seg_start = i;
                    if is_js_no_regex(word) {
                        can_regex = false;
                    }
                } else {
                    // check next non-space for '(' → function call
                    let mut j = i;
                    while j < len && bytes[j] == b' ' {
                        j += 1;
                    }
                    if j < len && bytes[j] == b'(' {
                        styled.push((STYLE_FUNCTION.clone(), word.to_string()));
                        seg_start = i;
                        // can_regex stays true (function calls allow regex)
                    } else if is_js_type(word) {
                        styled.push((STYLE_TYPE.clone(), word.to_string()));
                        seg_start = i;
                        // can_regex stays true (types allow regex)
                    } else {
                        styled.push((STYLE_IDENTIFIER.clone(), word.to_string()));
                        seg_start = i;
                        can_regex = false;
                    }
                }
                // cur_style remains DEFAULT for subsequent characters
            }

            _ => {
                can_regex = true;
                i += 1;
            }
        }
    }

    // flush remaining — use ERROR for unclosed strings/comments
    if in_single || in_double || in_backtick {
        if i > seg_start {
            styled.push((STYLE_ERROR.clone(), code[seg_start..i].to_string()));
        }
    } else if i > seg_start {
        styled.push((cur_style.clone(), code[seg_start..i].to_string()));
    }

    styled
}

fn is_js_keyword(w: &str) -> bool {
    matches!(
        w,
        "break"
            | "case"
            | "catch"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "while"
            | "with"
            | "class"
            | "const"
            | "enum"
            | "import"
            | "export"
            | "extends"
            | "super"
            | "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
            | "undefined"
            | "null"
            | "true"
            | "false"
            | "Infinity"
            | "NaN"
            | "eval"
            | "arguments"
            | "await"
    )
}

fn is_js_no_regex(w: &str) -> bool {
    matches!(
        w,
        "this"
            | "super"
            | "undefined"
            | "null"
            | "true"
            | "false"
            | "Infinity"
            | "NaN"
            | "arguments"
    )
}

fn is_js_type(w: &str) -> bool {
    matches!(w, "void" | "var")
}

// ---------------------------------------------------------------------------
// Multi-line input detection
// ---------------------------------------------------------------------------

/// Check whether `code` has balanced brackets/braces and closed strings/comments.
/// Returns `true` when the input is syntactically complete for evaluation.
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

// ---------------------------------------------------------------------------
// REPL helpers
// ---------------------------------------------------------------------------

fn history_path() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".aike_history"))
        .unwrap_or_else(|_| PathBuf::from(".aike_history"))
}

fn print_repl_help() {
    println!("Aike REPL commands:");
    println!("  /exit, /quit  exit the REPL");
    println!("  /help         show this help");
    println!();
    println!("JS evaluation:");
    println!("  Type any JavaScript expression and press Enter to evaluate.");
    println!("  Import statements (import * as X from '...') are supported via dynamic import().");
    println!("  Multi-line input is automatically detected (unbalanced brackets).");
    println!("  Press Ctrl-C to cancel multi-line input.");
    println!("  Press Ctrl-D to exit.");
}

fn add_history(editor: &mut Reedline, line: &str) {
    let item = HistoryItem::from_command_line(line);
    let _ = editor.history_mut().save(item);
}

// ---------------------------------------------------------------------------
// Import transformation
// ---------------------------------------------------------------------------

/// Transform static `import` statements into dynamic `import()` calls.
///
/// Static imports (e.g. `import * as fs from 'fs'`) run in module scope and don't
/// leak bindings to global scope.  We rewrite them as:
///   `const X = globalThis.X = await import('...')`
/// so that the imported namespace is accessible from subsequent REPL input.
fn transform_imports(code: &str) -> String {
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut pos = 0;
    let mut out = String::with_capacity(code.len() + 128);

    while pos < len {
        if pos + 7 > len || &bytes[pos..pos + 6] != b"import" {
            out.push(bytes[pos] as char);
            pos += 1;
            continue;
        }
        if pos > 0
            && (bytes[pos - 1].is_ascii_alphanumeric()
                || bytes[pos - 1] == b'_'
                || bytes[pos - 1] == b'$')
        {
            out.push(bytes[pos] as char);
            pos += 1;
            continue;
        }

        pos += 6;
        skip_ws_bytes(bytes, &mut pos, len);

        if pos >= len {
            break;
        }

        match bytes[pos] {
            b'{' => {
                let bindings = parse_named_bindings(bytes, &mut pos, len);
                skip_ws_bytes(bytes, &mut pos, len);
                let (spec, _) = parse_from_specifier(bytes, &mut pos, len);
                let temp = format!("__m{}", out.len());
                out.push_str(&format!("const {temp}=await import('{spec}');"));
                for (exported, local) in bindings {
                    out.push_str(&format!(
                        "const {local}=globalThis.{local}={temp}.{exported};"
                    ));
                }
            }
            b'*' => {
                pos += 1;
                skip_ws_bytes(bytes, &mut pos, len);
                if pos + 2 <= len && &bytes[pos..pos + 2] == b"as" {
                    pos += 2;
                    skip_ws_bytes(bytes, &mut pos, len);
                    let name = read_ident_str(bytes, &mut pos, len);
                    skip_ws_bytes(bytes, &mut pos, len);
                    let (spec, _) = parse_from_specifier(bytes, &mut pos, len);
                    out.push_str(&format!(
                        "const {name}=globalThis.{name}=await import('{spec}');"
                    ));
                }
            }
            b if b == b'\'' || b == b'"' => {
                let quote = b;
                pos += 1;
                while pos < len && bytes[pos] != quote {
                    pos += 1;
                }
                if pos < len {
                    pos += 1;
                }
            }
            _ => {
                let name = read_ident_str(bytes, &mut pos, len);
                skip_ws_bytes(bytes, &mut pos, len);
                let (spec, _) = parse_from_specifier(bytes, &mut pos, len);
                out.push_str(&format!(
                    "const {name}=globalThis.{name}=(await import('{spec}')).default;"
                ));
            }
        }
    }

    out
}

/// Advance `pos` past any consecutive whitespace bytes.
fn skip_ws_bytes(bytes: &[u8], pos: &mut usize, len: usize) {
    while *pos < len
        && (bytes[*pos] == b' '
            || bytes[*pos] == b'\t'
            || bytes[*pos] == b'\n'
            || bytes[*pos] == b'\r')
    {
        *pos += 1;
    }
}

/// Read an identifier (`[a-zA-Z0-9_$]+`) starting at `pos`.
fn read_ident_str(bytes: &[u8], pos: &mut usize, len: usize) -> String {
    let start = *pos;
    while *pos < len
        && (bytes[*pos].is_ascii_alphanumeric() || bytes[*pos] == b'_' || bytes[*pos] == b'$')
    {
        *pos += 1;
    }
    unsafe { std::str::from_utf8_unchecked(&bytes[start..*pos]) }.to_string()
}

/// Parse the `from '...'` clause after an import declaration.
/// Returns the module specifier string and the updated position.
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
            *pos += 1;
            return (spec, *pos);
        }
    }
    (String::new(), *pos)
}

/// Parse a `{ a, b as c }` import binding list.
/// Returns `Vec<(exported_name, local_name)>`.
fn parse_named_bindings(bytes: &[u8], pos: &mut usize, len: usize) -> Vec<(String, String)> {
    let mut bindings = Vec::new();
    if *pos >= len || bytes[*pos] != b'{' {
        return bindings;
    }
    *pos += 1;

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

// ---------------------------------------------------------------------------
// REPL main loop
// ---------------------------------------------------------------------------

/// Main REPL loop.
///
/// - Uses reedline for line editing, history, and syntax highlighting.
/// - Detects multi-line input via bracket/string balance.
/// - Transforms static imports to dynamic `import()` for global-scope evaluation.
/// - Handles Ctrl-C (cancel current input, double Ctrl-C to quit) and Ctrl-D.
pub(crate) async fn run_repl(rt: &mut Runtime, ctx: &mut TopLevelContext<'_>) -> ExitCode {
    let hist_path = history_path();
    let history = match FileBackedHistory::with_file(1000, hist_path) {
        Ok(h) => h,
        Err(_) => {
            FileBackedHistory::new(1000).unwrap_or_else(|_| FileBackedHistory::new(50).unwrap())
        }
    };
    let highlighter = AikePrompt {
        is_multiline: false,
    };
    let mut editor = Reedline::create()
        .with_history(Box::new(history))
        .with_highlighter(Box::new(highlighter));

    let mut prompt = AikePrompt {
        is_multiline: false,
    };
    let mut multi_line = String::new();
    let mut last_was_ctrl_c = false;

    println!("\x1b[32mWelcome to Aike REPL! Type /help for info.\x1b[0m");

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

                let trimmed = line.trim();
                // Only treat exact /exit /quit /help as commands,
                // so /ab/ (a regex literal) falls through to JS eval.
                if multi_line.is_empty() && matches!(trimmed, "/exit" | "/quit" | "/help") {
                    match trimmed {
                        "/exit" | "/quit" => break,
                        "/help" => print_repl_help(),
                        _ => unreachable!(),
                    }
                    add_history(&mut editor, trimmed);
                    continue;
                }

                multi_line.push_str(&line);
                multi_line.push('\n');

                if !is_input_complete(&multi_line) {
                    prompt.is_multiline = true;
                    continue;
                }

                prompt.is_multiline = false;
                let code = std::mem::take(&mut multi_line);
                let code_trimmed = code.trim().to_string();

                if code_trimmed.is_empty() {
                    continue;
                }

                add_history(&mut editor, &code_trimmed);

                let eval_code = transform_imports(&code_trimmed);

                let mut eval_error: Option<String> = None;

                ctx.with_new_scope(|mut scope| {
                    let r = (|| -> Result<(), i32> {
                        match scope.eval(
                            eval_code.as_bytes(),
                            "<repl>",
                            EvalType::Global,
                            EvalFlags::COMPILE_TO_ASYNC,
                        ) {
                            Ok(v) => {
                                // Check if the result is a Promise that's already settled.
                                // (COMPILE_TO_ASYNC wraps code in async IIFE; if the body
                                // throws synchronously, the Promise is rejected at eval time.)
                                if let Some(p) = v.as_object_Promise() {
                                    // Handle settled Promises entirely inside this scope
                                    match p.state() {
                                        PromiseState::Fulfilled => {
                                            let result = p.result();
                                            if let Ok(s) = result.inspect(&mut scope) {
                                                println!("{}", s);
                                            }
                                        }
                                        PromiseState::Rejected => {
                                            let reason = p.result();
                                            if let Ok(s) = reason.to_string(&mut scope) {
                                                colorize_println(s.encode_utf8(false), RedFg);
                                            }
                                        }
                                        PromiseState::Pending => {
                                            // Promise is still pending (e.g. code contains `await`).
                                            // Use into_future + event-loop to settle it.
                                            if let Ok(fut) = (*p).into_future(&mut scope) {
                                                let promise_res =
                                                    tokio::task::block_in_place(|| {
                                                        tokio::runtime::Handle::current().block_on(
                                                            async {
                                                                tokio::join!(rt.run_async(), fut)
                                                            },
                                                        )
                                                    })
                                                    .1;
                                                match promise_res {
                                                    Ok(resolved) => {
                                                        if let Ok(s) = resolved.inspect(&mut scope)
                                                        {
                                                            println!("{}", s);
                                                        }
                                                    }
                                                    Err(reason) => {
                                                        if let Ok(s) = reason.to_string(&mut scope)
                                                        {
                                                            colorize_println(
                                                                s.encode_utf8(false),
                                                                RedFg,
                                                            );
                                                        }
                                                    }
                                                }
                                            } else {
                                                // into_future failed, try to inspect directly
                                                if let Ok(s) = v.inspect(&mut scope) {
                                                    println!("{}", s);
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    // Non-Promise value, print it directly
                                    if let Ok(s) = v.inspect(&mut scope) {
                                        println!("{}", s);
                                    }
                                }
                            }
                            Err(_) => {
                                let exc = rt.exception();
                                match exc {
                                    Exception::Error(e) => {
                                        if let Ok(s) = e.to_string(&mut scope) {
                                            eval_error = Some(s.encode_utf8(false).to_string());
                                        }
                                    }
                                    Exception::Interrupted(_) => {
                                        eval_error = Some("interrupted".to_string());
                                    }
                                    Exception::OutOfMemory => {
                                        eval_error = Some("out of memory".to_string());
                                    }
                                    Exception::No | Exception::TooManyClasses => {}
                                }
                            }
                        }
                        Ok(())
                    })();
                    scope.return_value(r)
                });

                if let Some(err) = eval_error {
                    colorize_println(err, RedFg);
                    continue;
                }
            }
            _ => break,
        }
    }

    let _ = editor.history_mut().sync();
    println!();
    ExitCode::SUCCESS
}
