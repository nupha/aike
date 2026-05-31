// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::sync::{Arc, Mutex};

use reedline::{Completer, Hinter, Span, Suggestion};
use zuqe::{EvalFlags, EvalType, JSValue, ScopedContext};

/// Completion request sent from the readline thread to the main thread.
pub(crate) struct CompletionRequest {
    pub line: String,
    pub pos: usize,
}

/// Completion response sent from the main thread back to the readline thread.
pub(crate) struct CompletionResponse {
    pub suggestions: Vec<String>,
    /// Byte offset within `line` where the word being completed starts.
    pub word_start: usize,
}

/// All mutable state shared between the completer and the hinter, wrapped in
/// a single `Arc<Mutex<>>` so both can hold a cheap clone.
pub(crate) struct CompletionState {
    pub req_tx: tokio::sync::mpsc::UnboundedSender<CompletionRequest>,
    pub res_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionResponse>,
    /// The suffix portion of the hint (plain text, no ANSI).  Used by
    /// `complete_hint()` and `next_hint_token()`.  The completer derives the
    /// full completion string from `line[word_start..pos] + hint_suffix`.
    pub hint_suffix: Option<String>,
    /// Full candidate list for the completion menu popup.
    pub candidates: Vec<Suggestion>,
    /// Byte offset where the word being completed starts.
    pub word_start: usize,
    /// The cursor position at which the state was last populated.
    pub cached_pos: usize,
}

impl CompletionState {
    pub fn new(
        req_tx: tokio::sync::mpsc::UnboundedSender<CompletionRequest>,
        res_rx: tokio::sync::mpsc::UnboundedReceiver<CompletionResponse>,
    ) -> Self {
        Self {
            req_tx,
            res_rx,
            hint_suffix: None,
            candidates: Vec::new(),
            word_start: 0,
            cached_pos: 0,
        }
    }
}

/// Reedline `Completer` that uses the state populated by the hinter.
pub(crate) struct AikeCompleter {
    state: Arc<Mutex<CompletionState>>,
}

impl AikeCompleter {
    pub fn new(state: Arc<Mutex<CompletionState>>) -> Self {
        Self { state }
    }
}

impl Completer for AikeCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let mut state = self.state.lock().unwrap();

        // If the cursor moved since the last update, fetch fresh data.
        if pos != state.cached_pos {
            state
                .req_tx
                .send(CompletionRequest {
                    line: line.to_string(),
                    pos,
                })
                .ok();
            let resp = state.res_rx.blocking_recv();
            match resp {
                Some(r) if !r.suggestions.is_empty() => {
                    let prefix = &line[r.word_start..pos];
                    let span = Span::new(r.word_start, pos);
                    state.candidates = r
                        .suggestions
                        .iter()
                        .map(|v| Suggestion {
                            value: v.clone(),
                            span,
                            ..Suggestion::default()
                        })
                        .collect();
                    state.word_start = r.word_start;
                    state.cached_pos = pos;
                    state.hint_suffix = find_prefix_suffix(&r.suggestions, prefix);
                }
                _ => {
                    state.hint_suffix = None;
                    state.candidates.clear();
                    state.cached_pos = pos;
                    return Vec::new();
                }
            }
        }

        if let Some(ref suffix) = state.hint_suffix {
            let prefix = &line[state.word_start..pos];
            let full = format!("{}{}", prefix, suffix);
            let span = Span::new(state.word_start, pos);
            vec![Suggestion {
                value: full,
                span,
                ..Suggestion::default()
            }]
        } else if !state.candidates.is_empty() {
            let word_start = state.word_start;
            let candidates = state.candidates.clone();
            drop(state);
            let mut result = candidates;
            for s in &mut result {
                s.span = Span::new(word_start, pos);
            }
            result
        } else {
            Vec::new()
        }
    }
}

/// Reedline `Hinter` that shows a dimmed inline hint when one candidate is a
/// prefix of another (e.g. `readFile` → `readFileSync`).
pub(crate) struct AikeHinter {
    state: Arc<Mutex<CompletionState>>,
}

impl AikeHinter {
    pub fn new(state: Arc<Mutex<CompletionState>>) -> Self {
        Self { state }
    }
}

impl Hinter for AikeHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        _history: &dyn reedline::History,
        _use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        if line.is_empty() || pos == 0 {
            let mut state = self.state.lock().unwrap();
            state.hint_suffix = None;
            state.candidates.clear();
            state.word_start = 0;
            state.cached_pos = pos;
            return String::new();
        }

        let mut state = self.state.lock().unwrap();
        state
            .req_tx
            .send(CompletionRequest {
                line: line.to_string(),
                pos,
            })
            .ok();

        let resp = match state.res_rx.blocking_recv() {
            Some(r) if !r.suggestions.is_empty() => r,
            _ => {
                state.hint_suffix = None;
                state.candidates.clear();
                state.cached_pos = pos;
                return String::new();
            }
        };

        let prefix = &line[resp.word_start..pos];

        let span = Span::new(resp.word_start, pos);
        state.candidates = resp
            .suggestions
            .iter()
            .map(|v| Suggestion {
                value: v.clone(),
                span,
                ..Suggestion::default()
            })
            .collect();
        state.word_start = resp.word_start;
        state.cached_pos = pos;

        let hint_suffix = find_prefix_suffix(&resp.suggestions, prefix);
        state.hint_suffix = hint_suffix.clone();

        if let Some(ref suffix) = hint_suffix {
            format!("\x1b[2m{}\x1b[0m", suffix)
        } else {
            String::new()
        }
    }

    fn complete_hint(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .hint_suffix
            .clone()
            .unwrap_or_default()
    }

    fn next_hint_token(&self) -> String {
        let hint = self.complete_hint();
        hint.split_once(|c: char| c.is_whitespace())
            .map(|(token, _)| token.to_string())
            .unwrap_or(hint)
    }
}

/// If one candidate is a strict prefix of another, return the suffix after
/// `prefix` from the shorter (prefix) candidate.  Otherwise return `None`.
///
/// Example: candidates=["Array","ArrayBuffer"], prefix="Arr"
///   → Some("ay")   (because "Array" is a prefix of "ArrayBuffer")
///
/// Example: candidates=["Array","ArrayBuffer"], prefix="Array"
///   → Some("Buffer")
fn find_prefix_suffix(candidates: &[String], prefix: &str) -> Option<String> {
    let prefix_candidates: Vec<&str> = candidates
        .iter()
        .map(|s| s.as_str())
        .filter(|s| {
            candidates
                .iter()
                .any(|other| other.as_str() != *s && other.starts_with(*s))
        })
        .collect();
    let target = prefix_candidates
        .iter()
        .find(|s| s.len() > prefix.len() && s.starts_with(prefix))?;
    Some(target[prefix.len()..].to_string())
}

/// Given a line and cursor position, compute completions using zuqe.
///
/// Mirrors the logic from `repl.js` `get_completions()`:
/// 1. Extract the word at cursor
/// 2. Find the object before the dot (if any)
/// 3. Enumerate its own properties + prototype chain
/// 4. Filter by prefix match
pub(crate) fn js_compute_completions(
    scope: &mut ScopedContext,
    line: &str,
    pos: usize,
) -> (Vec<String>, usize) {
    /// Check if `c` is a JS word character (alphanumeric, _, $).
    fn is_word_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
    }

    let bytes = line.as_bytes();

    let mut word_start = pos;
    while word_start > 0 && is_word_char(bytes[word_start - 1]) {
        word_start -= 1;
    }
    let prefix = &line[word_start..pos];

    let ctx_obj = if word_start >= 2 && bytes[word_start - 1] == b'.' {
        let ident_end = word_start - 1;
        let mut ident_start = ident_end;
        while ident_start > 0 && is_word_char(bytes[ident_start - 1]) {
            ident_start -= 1;
        }
        let ident = &line[ident_start..ident_end];
        match scope.eval(
            ident.as_bytes(),
            "<completion>",
            EvalType::Global,
            EvalFlags::empty(),
        ) {
            Ok(v) => v,
            Err(_) => return (Vec::new(), word_start),
        }
    } else {
        if word_start > 0 && is_word_char(bytes[word_start - 1]) {
            return (Vec::new(), word_start);
        }
        scope.global_obj().into()
    };

    let mut props: Vec<String> = Vec::new();
    let mut obj = ctx_obj;

    for _ in 0..10 {
        let Some(o) = obj.as_object_mut() else {
            break;
        };
        let pks = match o.get_own_property_names(scope, 1) {
            Ok(p) => p,
            Err(_) => break,
        };
        for pk in pks.iter() {
            if pk.atom.to_array_index(scope.rt()).is_some() {
                continue;
            }
            let Ok(name_val) = pk.atom.to_value(scope) else {
                continue;
            };
            let Ok(name_str) = name_val.to_string(scope) else {
                continue;
            };
            let name_str = name_str.content().encode_utf8(false).to_string();
            if name_str.starts_with(prefix) {
                props.push(name_str);
            }
        }
        let proto = match o.shape().and_then(|s| s.proto()) {
            Some(p) => p,
            None => break,
        };
        obj = JSValue::from(proto);
    }

    props.sort_by(|a, b| {
        let a_underscore = a.starts_with('_');
        let b_underscore = b.starts_with('_');
        if a_underscore != b_underscore {
            return if a_underscore {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            };
        }
        a.cmp(b)
    });

    props.dedup();

    (props, word_start)
}
