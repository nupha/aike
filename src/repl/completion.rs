// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use rustyline::{
    Context,
    completion::{Completer, Pair},
    highlight::Highlighter,
    hint::Hinter,
    validate::Validator,
};
use rustyline_derive::Helper;
use zuqe::JSValue;

use crate::repl::prompt::AikePrompt;

type CandidatePrinter = Arc<dyn Fn(&[CompletionCandidate]) + Send + Sync>;

/// A completion candidate returned by the JS engine.
#[derive(Debug, Clone)]
pub(crate) struct CompletionCandidate {
    pub(crate) name: String,
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
}

/// State shared between the REPL and completion system.
#[derive(Debug, Clone)]
pub(crate) struct CompletionState {
    pub(crate) req_tx: tokio::sync::mpsc::UnboundedSender<CompletionRequest>,
    pub(crate) req_rx:
        Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CompletionRequest>>>,
    /// Cached hint suffix for the Hinter.
    pub(crate) hint_cache: Arc<parking_lot::Mutex<Option<String>>>,
    /// Line for which the hint was cached (staleness guard).
    pub(crate) hint_line: Arc<parking_lot::Mutex<Option<String>>>,

    // -- history navigation state -----------------------------------------
    /// True while the user is actively scrolling history (started from empty
    /// input).  Reset when `AikeHelper::hint()` detects a user keystroke.
    pub(crate) nav_active: Arc<AtomicBool>,
    /// Incremented by `HistNav` each time it dispatches an unconditional
    /// navigation command.  Used by `hint()` to distinguish "redraw caused
    /// by navigation" from "redraw caused by user typing".
    pub(crate) nav_event: Arc<AtomicU64>,
    /// Last `nav_event` value consumed by `hint()`.  Set on the first
    /// `hint()` call after a navigation event; used to detect the SECOND
    /// `hint()` call (if any) so we don't incorrectly reset `nav_active`.
    pub(crate) hint_nav_seen: Arc<AtomicU64>,
}

impl CompletionState {
    pub(crate) fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            req_tx: tx,
            req_rx: Arc::new(tokio::sync::Mutex::new(rx)),
            hint_cache: Arc::new(parking_lot::Mutex::new(None)),
            hint_line: Arc::new(parking_lot::Mutex::new(None)),
            nav_active: Arc::new(AtomicBool::new(false)),
            nav_event: Arc::new(AtomicU64::new(0)),
            hint_nav_seen: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn clear_hint(&self) {
        *self.hint_cache.lock() = None;
        *self.hint_line.lock() = None;
    }
}

// ---------------------------------------------------------------------------
// Helper — combines Completer, Hinter, Highlighter, Validator
// ---------------------------------------------------------------------------

#[derive(Helper)]
pub(crate) struct AikeHelper {
    pub(crate) prompt: AikePrompt,
    pub(crate) completion_state: CompletionState,
    pub(crate) print_candidates: Option<CandidatePrinter>,
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

        if hint_suffix.is_some() {
            // A common prefix exists among candidates. Return all matching
            // candidates as-is — rustyline's List mode will automatically
            // complete to the longest common prefix and display the list.
            let pairs: Vec<Pair> = resp
                .candidates
                .iter()
                .filter(|c| c.name.starts_with(prefix))
                .map(|c| Pair {
                    display: c.name.clone(),
                    replacement: c.name.clone(),
                })
                .collect();
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
        // Detect navigation-vs-typing: if nav_event changed since the last
        // hint() call, the redraw was triggered by a navigation command
        // (PreviousHistory/NextHistory); otherwise by a user keystroke.
        let nav = self.completion_state.nav_event.load(Ordering::Acquire);
        let seen = self
            .completion_state
            .hint_nav_seen
            .swap(nav, Ordering::AcqRel);
        if nav == seen && self.completion_state.nav_active.load(Ordering::Acquire) {
            // No new nav event since last hint → user typed.
            self.completion_state
                .nav_active
                .store(false, Ordering::Release);
        }

        if line.is_empty() || pos == 0 {
            self.completion_state.clear_hint();
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
                    self.completion_state.clear_hint();
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
                    self.completion_state.clear_hint();
                    return None;
                }
                // Cache the hint + the line it was computed for
                *self.completion_state.hint_cache.lock() = Some(suffix.clone());
                *self.completion_state.hint_line.lock() = Some(line.to_string());
                Some(suffix)
            }
            Err(_) => {
                // Timeout: only return cached hint if line hasn't changed
                // (e.g. after history navigation the line changes but the
                // stale cached hint would be wrong).
                if self.completion_state.hint_line.lock().as_deref() != Some(line) {
                    return None;
                }
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

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        self.prompt.highlight_hint(hint)
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
            // common_len may land mid-character if the differing byte is a
            // continuation byte of a multi-byte UTF-8 sequence. Walk back to
            // the nearest char boundary to avoid panicking on &str slice.
            let safe_len = (0..=common_len.min(a.len()))
                .rev()
                .find(|&i| a.is_char_boundary(i))
                .unwrap_or(0);
            &a[..safe_len]
        })
        .unwrap();

    if common.len() > prefix.len() {
        Some(common[prefix.len()..].to_string())
    } else {
        None
    }
}

/// Resolve the object before the cursor by recursively walking the property path.
///
/// Mirrors `get_context_object()` from `repl.js`:
/// For `Aike.stdout.`, evaluates `Aike`, then accesses `.stdout` on the result.
/// Returns `JSValue::UNDEFINED` if resolution fails.
fn resolve_context_obj(scope: &mut zuqe::ScopedContext<'_>, line: &str, pos: usize) -> JSValue {
    fn is_word_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
    }

    let bytes = line.as_bytes();

    // At start or preceded by an operator character → global object
    if pos == 0 || b" ~!%^&*(-+={[|:;,<>?/".contains(&bytes[pos - 1]) {
        return scope.global_obj().into();
    }

    // Preceded by a dot → resolve the left-hand side and access the property
    if pos >= 2 && bytes[pos - 1] == b'.' {
        let prop_end = pos - 1;
        let mut prop_start = prop_end;
        while prop_start > 0 && is_word_char(bytes[prop_start - 1]) {
            prop_start -= 1;
        }
        let prop_name = &line[prop_start..prop_end];

        // Recursively resolve the object before the dot
        let obj = resolve_context_obj(scope, line, prop_start);
        if obj.is_undefined_or_null() {
            return JSValue::UNDEFINED;
        }
        let Some(o) = obj.as_object() else {
            return JSValue::UNDEFINED;
        };
        match o.get_property_str(scope, prop_name) {
            Ok(v) => v,
            Err(_) => {
                scope.take_exception();
                JSValue::UNDEFINED
            }
        }
    } else {
        // Preceded by a word character but no dot → cursor is mid-word, can't resolve
        if is_word_char(bytes[pos - 1]) {
            return JSValue::UNDEFINED;
        }
        scope.global_obj().into()
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

    let ctx_obj = resolve_context_obj(scope, &req.line, word_start);
    if ctx_obj.is_undefined() {
        let _ = req.resp_tx.send(CompletionResponse {
            candidates: Vec::new(),
            start: word_start,
        });
        return;
    }

    let mut props: Vec<String> = Vec::new();
    let mut obj = ctx_obj;

    for _ in 0..10 {
        let Some(mut o) = obj.as_object() else {
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
        let proto = match o.prototype(scope) {
            Ok(Some(p)) => p,
            _ => break,
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
        .map(|name| CompletionCandidate { name })
        .collect();

    let _ = req.resp_tx.send(CompletionResponse {
        candidates,
        start: word_start,
    });
}
