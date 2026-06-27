// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::borrow::Cow;

use nu_ansi_term::Style;
use rustyline::{
    Prompt,
    highlight::{CmdKind, Highlighter},
};

/// Default text / identifier color (bright_green).
static STYLE_DEFAULT: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::LightGreen));

/// Comment (white).
static STYLE_COMMENT: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::White));

/// String literal (bright_cyan).
static STYLE_STRING: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::Cyan));

/// Regex literal (cyan).
static STYLE_REGEX: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::Cyan));

/// Number literal (green).
static STYLE_NUMBER: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::Green));

/// Keyword (bright_white → LightGray in nu_ansi_term).
static STYLE_KEYWORD: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::LightGray));

/// Function call (bright_yellow).
static STYLE_FUNCTION: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::LightYellow));

/// Type annotation (bright_magenta).
static STYLE_TYPE: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::LightMagenta));

/// Identifier (bright_green — same as default).
static STYLE_IDENTIFIER: std::sync::LazyLock<Style> =
    std::sync::LazyLock::new(|| Style::new().fg(nu_ansi_term::Color::LightGreen));

/// Error (red).
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
    fn raw(&self) -> &str {
        if self.is_multiline {
            "   ... "
        } else {
            "aike> "
        }
    }

    fn styled(&self) -> &str {
        if self.is_multiline {
            "   ... "
        } else {
            "\x1b[32maike>\x1b[0m "
        }
    }
}

impl Highlighter for AikePrompt {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Owned(colorize_js(line))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        // Dim style for completion hints.
        // Note: display() returns plain text for position calculation
        // (required on Windows where calculate_position asserts no ANSI codes).
        // The ANSI dim styling is applied here so it only affects rendering.
        Cow::Owned(format!("\x1b[2m{}\x1b[0m", hint))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _cmd: CmdKind) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Syntax colouring — ported from repl.js colorize_js()
// ---------------------------------------------------------------------------

#[allow(unused_assignments)]
fn colorize_js(code: &str) -> String {
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut out = String::new();
    let mut seg_start: usize = 0;
    let mut cur_style: &Style = &STYLE_DEFAULT;
    let mut i = 0;

    // state tracking
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut in_block_comment = false;
    let mut can_regex = true;

    macro_rules! flush {
        () => {{
            if i > seg_start {
                out.push_str(&cur_style.paint(&code[seg_start..i]).to_string());
            }
            seg_start = i;
        }};
    }

    macro_rules! emit_token {
        ($st:expr, $token:expr) => {{
            if seg_start < i {
                out.push_str(&cur_style.paint(&code[seg_start..i]).to_string());
            }
            out.push_str(&$st.paint($token).to_string());
            seg_start = i;
        }};
    }

    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';

    while i < len {
        let c = bytes[i];

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

        if in_block_comment {
            if c == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                let end = i + 2;
                if end > seg_start {
                    out.push_str(&STYLE_COMMENT.paint(&code[seg_start..end]).to_string());
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
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,

            b'/' => {
                if i + 1 < len {
                    match bytes[i + 1] {
                        b'*' => {
                            flush!();
                            cur_style = &STYLE_COMMENT;
                            i += 2;
                            in_block_comment = true;
                        }
                        b'/' => {
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
                            can_regex = false;
                        }
                        _ => {
                            i += 1;
                            can_regex = true;
                        }
                    }
                } else {
                    i += 1;
                }
            }

            b'\'' => {
                emit_token!(&*STYLE_STRING, &code[i..i + 1]);
                i += 1;
                seg_start = i;
                cur_style = &STYLE_STRING;
                in_single = true;
                can_regex = false;
            }
            b'"' => {
                emit_token!(&*STYLE_STRING, &code[i..i + 1]);
                i += 1;
                seg_start = i;
                cur_style = &STYLE_STRING;
                in_double = true;
                can_regex = false;
            }
            b'`' => {
                emit_token!(&*STYLE_STRING, &code[i..i + 1]);
                i += 1;
                seg_start = i;
                cur_style = &STYLE_STRING;
                in_backtick = true;
                can_regex = false;
            }

            b'+' | b'-' => {
                if i + 1 < len && bytes[i + 1] == c {
                    i += 2;
                    continue;
                }
                i += 1;
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
                can_regex = false;
            }

            _ if is_word(c) => {
                let id_start = i;
                flush!();
                while i < len && is_word(bytes[i]) {
                    i += 1;
                }
                let word = &code[id_start..i];

                can_regex = true;

                if js_is_keyword(word) {
                    out.push_str(&STYLE_KEYWORD.paint(word).to_string());
                    seg_start = i;
                    if is_js_no_regex(word) {
                        can_regex = false;
                    }
                } else {
                    let mut j = i;
                    while j < len && bytes[j] == b' ' {
                        j += 1;
                    }
                    if j < len && bytes[j] == b'(' {
                        out.push_str(&STYLE_FUNCTION.paint(word).to_string());
                        seg_start = i;
                    } else if is_js_type(word) {
                        out.push_str(&STYLE_TYPE.paint(word).to_string());
                        seg_start = i;
                    } else {
                        out.push_str(&STYLE_IDENTIFIER.paint(word).to_string());
                        seg_start = i;
                        can_regex = false;
                    }
                }
            }

            _ => {
                can_regex = true;
                i += 1;
            }
        }
    }

    if in_single || in_double || in_backtick {
        if i > seg_start {
            out.push_str(&STYLE_ERROR.paint(&code[seg_start..i]).to_string());
        }
    } else if i > seg_start {
        out.push_str(&cur_style.paint(&code[seg_start..i]).to_string());
    }

    out
}

fn js_is_keyword(w: &str) -> bool {
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
