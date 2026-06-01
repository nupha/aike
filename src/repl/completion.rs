// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use rustyline::Context;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline_derive::Helper;

use crate::repl::prompt::AikePrompt;

// ---------------------------------------------------------------------------
// Completion types
// ---------------------------------------------------------------------------

/// A completion candidate returned by the JS engine.
#[derive(Debug, Clone)]
pub(crate) struct CompletionCandidate {
    pub(crate) name: String,
    pub(crate) kind: String,
}

/// Request sent to the JS thread for completions.
#[derive(Debug)]
pub(crate) struct CompletionRequest {
    pub(crate) line: String,
    pub(crate) pos: usize,
    pub(crate) resp_tx: std::sync::mpsc::Sender<CompletionResponse>,
}

/// Response from the JS thread.
#[derive(Debug)]
pub(crate) struct CompletionResponse {
    pub(crate) candidates: Vec<CompletionCandidate>,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// State shared between the REPL and completion system.
#[derive(Debug, Clone)]
pub(crate) struct CompletionState {
    pub(crate) req_tx: tokio::sync::mpsc::UnboundedSender<CompletionRequest>,
    pub(crate) req_rx:
        Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CompletionRequest>>>,
    /// Cached hint suffix for the Hinter.
    pub(crate) hint_cache: Arc<parking_lot::Mutex<Option<String>>>,
}

impl CompletionState {
    pub(crate) fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            req_tx: tx,
            req_rx: Arc::new(tokio::sync::Mutex::new(rx)),
            hint_cache: Arc::new(parking_lot::Mutex::new(None)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper — combines Completer, Hinter, Highlighter, Validator
// ---------------------------------------------------------------------------

#[derive(Helper)]
pub(crate) struct AikeHelper {
    pub(crate) prompt: AikePrompt,
    pub(crate) completion_state: CompletionState,
    pub(crate) print_candidates: Option<Arc<dyn Fn(&[CompletionCandidate]) + Send + Sync>>,
}

impl AikeHelper {
    pub(crate) fn new(completion_state: CompletionState) -> Self {
        Self {
            prompt: AikePrompt {
                is_multiline: false,
            },
            completion_state,
            print_candidates: None,
        }
    }
}

impl Completer for AikeHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (resp_tx, resp_rx) = std::sync::mpsc::channel();

        if self
            .completion_state
            .req_tx
            .send(CompletionRequest {
                line: line.to_string(),
                pos,
                resp_tx,
            })
            .is_err()
        {
            return Ok((0, Vec::new()));
        }

        let resp = match resp_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => return Ok((0, Vec::new())),
        };

        if resp.candidates.is_empty() {
            return Ok((0, Vec::new()));
        }

        // Check if there's a common prefix hint. If so, complete to the common
        // prefix first (e.g. "re" + Tab → "readFile" when candidates are
        // ["readFile", "readFileSync"]).
        let prefix = &line[resp.start..pos];
        let hint_suffix = find_prefix_suffix(
            &resp
                .candidates
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
            prefix,
        );

        if let Some(suffix) = hint_suffix {
            // Complete to the common prefix first, then include all
            // matching candidates for subsequent Tab cycling.
            let full = format!("{}{}", prefix, suffix);
            let mut pairs = vec![Pair {
                display: full.clone(),
                replacement: full.clone(),
            }];
            // Add remaining candidates that share this prefix for cycling
            for c in &resp.candidates {
                if c.name != full && c.name.starts_with(&full) {
                    pairs.push(Pair {
                        display: c.name.clone(),
                        replacement: c.name.clone(),
                    });
                }
            }
            return Ok((resp.start, pairs));
        }

        // When multiple candidates exist with no common prefix, only print the
        // list without modifying the input line (Node.js REPL style).
        if resp.candidates.len() >= 2 {
            if let Some(ref print_fn) = self.print_candidates {
                print_fn(&resp.candidates);
            }
            return Ok((0, Vec::new()));
        }

        let pairs: Vec<Pair> = resp
            .candidates
            .iter()
            .map(|c| Pair {
                display: c.name.clone(),
                replacement: c.name.clone(),
            })
            .collect();

        Ok((resp.start, pairs))
    }
}

// ---------------------------------------------------------------------------
// Hinter
// ---------------------------------------------------------------------------

impl Hinter for AikeHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if line.is_empty() || pos == 0 {
            return None;
        }

        // Send a hint request to the JS thread (non-blocking).
        // The JS thread will compute completions and cache the hint suffix.
        let (resp_tx, resp_rx) = std::sync::mpsc::channel();
        let _ = self.completion_state.req_tx.send(CompletionRequest {
            line: line.to_string(),
            pos,
            resp_tx,
        });

        // Wait briefly for the hint result (timeout to avoid blocking input).
        match resp_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(resp) => {
                if resp.candidates.is_empty() {
                    return None;
                }
                let prefix = &line[resp.start..pos];
                let suffix = find_prefix_suffix(
                    &resp
                        .candidates
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>(),
                    prefix,
                )?;
                if suffix.is_empty() {
                    return None;
                }
                // Cache the hint
                *self.completion_state.hint_cache.lock() = Some(suffix.clone());
                Some(format!("\x1b[2m{}\x1b[0m", suffix))
            }
            Err(_) => {
                // Timeout: return cached hint if available
                self.completion_state.hint_cache.lock().clone()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Highlighter — delegates to AikePrompt
// ---------------------------------------------------------------------------

impl Highlighter for AikeHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.prompt.highlight(line, pos)
    }

    fn highlight_char(&self, line: &str, pos: usize, cmd: rustyline::highlight::CmdKind) -> bool {
        self.prompt.highlight_char(line, pos, cmd)
    }
}

// ---------------------------------------------------------------------------
// Validator — always valid (no multi-line validation yet)
// ---------------------------------------------------------------------------

impl Validator for AikeHelper {
    fn validate(
        &self,
        _ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        Ok(rustyline::validate::ValidationResult::Valid(None))
    }
}

// ---------------------------------------------------------------------------
// JS completion bridge
// ---------------------------------------------------------------------------

/// Find the longest common prefix suffix among candidates that start with `prefix`.
fn find_prefix_suffix(candidates: &[String], prefix: &str) -> Option<String> {
    let matching: Vec<&str> = candidates
        .iter()
        .map(|s| s.as_str())
        .filter(|s| s.starts_with(prefix) && s.len() > prefix.len())
        .collect();

    if matching.is_empty() {
        return None;
    }

    let common = matching
        .iter()
        .copied()
        .reduce(|a, b| {
            let common_len = a
                .as_bytes()
                .iter()
                .zip(b.as_bytes())
                .take_while(|(x, y)| x == y)
                .count();
            &a[..common_len]
        })
        .unwrap();

    if common.len() > prefix.len() {
        Some(common[prefix.len()..].to_string())
    } else {
        None
    }
}

/// Compute completions by calling the JS engine.
/// Called from the JS thread when a completion request is detected.
pub(crate) fn js_compute_completions(scope: &mut zuqe::ScopedContext<'_>, req: CompletionRequest) {
    fn is_word_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
    }

    let bytes = req.line.as_bytes();
    let pos = req.pos;

    let mut word_start = pos;
    while word_start > 0 && is_word_char(bytes[word_start - 1]) {
        word_start -= 1;
    }
    let prefix = &req.line[word_start..pos];

    let ctx_obj = if word_start >= 2 && bytes[word_start - 1] == b'.' {
        let ident_end = word_start - 1;
        let mut ident_start = ident_end;
        while ident_start > 0 && is_word_char(bytes[ident_start - 1]) {
            ident_start -= 1;
        }
        let ident = &req.line[ident_start..ident_end];
        match scope.eval(
            ident.as_bytes(),
            "<completion>",
            zuqe::EvalType::Global,
            zuqe::EvalFlags::empty(),
        ) {
            Ok(v) => v,
            Err(_) => {
                let _ = req.resp_tx.send(CompletionResponse {
                    candidates: Vec::new(),
                    start: word_start,
                    end: pos,
                });
                return;
            }
        }
    } else {
        if word_start > 0 && is_word_char(bytes[word_start - 1]) {
            let _ = req.resp_tx.send(CompletionResponse {
                candidates: Vec::new(),
                start: word_start,
                end: pos,
            });
            return;
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
        obj = zuqe::JSValue::from(proto);
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

    let candidates: Vec<CompletionCandidate> = props
        .into_iter()
        .map(|name| CompletionCandidate {
            name,
            kind: String::new(),
        })
        .collect();

    let _ = req.resp_tx.send(CompletionResponse {
        candidates,
        start: word_start,
        end: pos,
    });
}
