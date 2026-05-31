// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use reedline::{Completer, Span, Suggestion};
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

/// A reedline `Completer` that forwards completion requests to the main thread
/// via a channel, where zuqe can evaluate JS expressions to enumerate properties.
pub(crate) struct AikeCompleter {
    pub req_tx: std::sync::mpsc::Sender<CompletionRequest>,
    pub resp_rx: std::sync::mpsc::Receiver<CompletionResponse>,
}

impl Completer for AikeCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        if line.is_empty() || pos == 0 {
            return Vec::new();
        }

        let req = CompletionRequest {
            line: line.to_string(),
            pos,
        };

        if self.req_tx.send(req).is_err() {
            return Vec::new();
        }

        let resp = match self.resp_rx.recv() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        if resp.suggestions.is_empty() {
            return Vec::new();
        }

        let span = Span::new(resp.word_start, pos);
        resp.suggestions
            .into_iter()
            .map(|value| Suggestion {
                value,
                span,
                ..Suggestion::default()
            })
            .collect()
    }
}

/// Check if `c` is a JS word character (alphanumeric, _, $).
fn is_js_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// Given a line and cursor position, compute completions using zuqe.
///
/// Mirrors the logic from `repl.js` `get_completions()`:
/// 1. Extract the word at cursor
/// 2. Find the object before the dot (if any)
/// 3. Enumerate its own properties + prototype chain
/// 4. Filter by prefix match
pub(crate) fn compute_completions(
    scope: &mut ScopedContext,
    line: &str,
    pos: usize,
) -> (Vec<String>, usize) {
    let bytes = line.as_bytes();

    let mut word_start = pos;
    while word_start > 0 && is_js_word_char(bytes[word_start - 1]) {
        word_start -= 1;
    }
    let prefix = &line[word_start..pos];

    let ctx_obj = if word_start >= 2 && bytes[word_start - 1] == b'.' {
        let ident_end = word_start - 1;
        let mut ident_start = ident_end;
        while ident_start > 0 && is_js_word_char(bytes[ident_start - 1]) {
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
        if word_start > 0 && is_js_word_char(bytes[word_start - 1]) {
            return (Vec::new(), word_start);
        }
        scope.global_obj().into()
    };

    let mut props: Vec<String> = Vec::new();
    let mut obj = ctx_obj;

    for _ in 0..10 {
        let Some(mut o) = obj.as_object_mut() else {
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
