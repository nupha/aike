// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::borrow::Cow;

use nu_ansi_term::Style;
use reedline::{Highlighter, Prompt, PromptEditMode, PromptHistorySearch, StyledText};

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

    macro_rules! flush {
        () => {{
            if i > seg_start {
                styled.push((cur_style.clone(), code[seg_start..i].to_string()));
            }
            seg_start = i;
        }};
    }

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
                        }
                        _ => {
                            i += 1;
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

            _ if is_word(c) => {
                let id_start = i;
                flush!();
                while i < len && is_word(bytes[i]) {
                    i += 1;
                }
                let word = &code[id_start..i];

                can_regex = true;

                if is_js_keyword(word) {
                    styled.push((STYLE_KEYWORD.clone(), word.to_string()));
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
                        styled.push((STYLE_FUNCTION.clone(), word.to_string()));
                        seg_start = i;
                    } else if is_js_type(word) {
                        styled.push((STYLE_TYPE.clone(), word.to_string()));
                        seg_start = i;
                    } else {
                        styled.push((STYLE_IDENTIFIER.clone(), word.to_string()));
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
