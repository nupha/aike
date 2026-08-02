// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    process::ExitCode,
    sync::{
        Arc,
        atomic::Ordering::{self, Acquire, Release},
    },
};

use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, EventHandler, KeyCode, KeyEvent,
    Modifiers, RepeatCount,
    config::Builder as ConfigBuilder,
    config::{CompletionType, Configurer},
    error::ReadlineError,
    history::{FileHistory, History, SearchDirection},
};

use crate::repl::{
    IN_READLINE,
    completion::{AikeHelper, CompletionState, js_compute_completions},
    init_printer, print_output,
    prompt::AikePrompt,
};

use zuqe::ExitStatus;

/// Commands sent from the readline thread to the JS thread.
#[derive(Debug)]
pub(crate) enum InputCommand {
    Eval(String),
    Quit,
}

/// Conditional Up/Down handler: navigates history unconditionally
/// (PreviousHistory/NextHistory) while the user is flipping through history,
/// but switches to prefix-filtered search (HistorySearchBackward/Forward)
/// once the user types on the input.
///
/// The transition is detected via a shared `nav_event` counter: each time
/// we dispatch an unconditional nav command we bump the counter, and the
/// `Hinter::hint` callback (which fires after every display refresh) uses
/// the counter to distinguish "redraw from user keystroke" from "redraw
/// from history navigation".
struct HistNav {
    completion_state: CompletionState,
    /// The rustyline command for prefix search (HistorySearchBackward or Forward).
    search_cmd: Cmd,
}

impl HistNav {
    fn new(completion_state: CompletionState, search_cmd: Cmd) -> Self {
        Self {
            completion_state,
            search_cmd,
        }
    }
}

impl ConditionalEventHandler for HistNav {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        self.completion_state.clear_hint();
        let line = ctx.line();
        if line.is_empty() || self.completion_state.nav_active.load(Acquire) {
            // Empty input, or we're mid-navigation: keep unconditional nav.
            self.completion_state.nav_active.store(true, Release);
            self.completion_state.nav_event.fetch_add(1, Release);
            Some(match self.search_cmd {
                Cmd::HistorySearchBackward => Cmd::PreviousHistory,
                Cmd::HistorySearchForward => Cmd::NextHistory,
                _ => self.search_cmd.clone(),
            })
        } else {
            // Non-empty line and nav_active is false → user typed → prefix search.
            Some(self.search_cmd.clone())
        }
    }
}

/// Check if `line` already exists anywhere in the history (global dedup).
fn history_contains(hist: &impl History, line: &str) -> bool {
    for i in 0..hist.len() {
        if let Ok(Some(result)) = hist.get(i, SearchDirection::Forward) {
            if result.entry.as_ref() == line {
                return true;
            }
        }
    }
    false
}

fn process_input_line(
    ctx: &mut zuqe::Context<'_>,
    line: &str,
    ack_tx: &std::sync::mpsc::Sender<()>,
) {
    match line {
        ".help" => {
            print_output(HELP.into());
        }
        _ => {
            match ctx.eval(
                line.as_bytes(),
                zuqe::EvalSource::Name("<repl>".to_string()),
                zuqe::EvalType::Global,
                zuqe::EvalFlags::empty(),
            ) {
                Ok(val) => {
                    let t = crate::inspect_value::js_inspect(&mut ctx.root_scope(), val);
                    print_output(t.into());
                }
                Err(_) => {
                    let t = ctx.take_exception().to_text(&mut ctx.root_scope());
                    let mut msg = t.message;
                    if let Some(tb) = t.traceback {
                        msg.push('\n');
                        msg.push_str(&tb);
                    }
                    print_output(format!("\x1b[31m{}\x1b[0m", msg).into());
                }
            }

            let _ = ack_tx.send(());
        }
    }
}

/// Start the REPL event loop. Returns the process exit code.
pub(crate) async fn run<'c>(mut rt: zuqe::Runtime, mut ctx0: zuqe::Context<'c>) -> ExitCode {
    let completion_state = CompletionState::new();

    // Build rustyline editor.  Call set_history_ignore_dups first (it takes
    // &mut self); then chain consuming builder methods.
    let mut builder = ConfigBuilder::new();
    let _ = builder.set_history_ignore_dups(false);
    let config = builder
        .auto_add_history(false)
        .completion_type(CompletionType::List)
        .build();

    let mut editor =
        Editor::<AikeHelper, FileHistory>::with_config(config).expect("Editor::with_config");

    // Create ExternalPrinter and register it globally.
    let printer = Box::new(
        editor
            .create_external_printer()
            .expect("create_external_printer"),
    );
    init_printer(printer);

    let mut helper = AikeHelper::new(completion_state.clone());
    helper.print_candidates = Some({
        Arc::new(move |candidates| {
            let term_width = crossterm::terminal::size()
                .map(|(w, _)| w as usize)
                .unwrap_or(80);
            let max_width = candidates.iter().map(|c| c.name.len()).max().unwrap_or(0) + 2;
            let n_cols = std::cmp::max(1, (term_width + 1) / max_width);
            let n_rows = candidates.len().div_ceil(n_cols);

            // Print candidate list below the input line using ExternalPrinter.
            // ExternalPrinter saves the current input line, prints the message,
            // then restores the input line. The leading \n ensures the list
            // appears below the input line rather than on top of it.
            let mut out = String::new();
            out.push('\n');
            for row in 0..n_rows {
                for col in 0..n_cols {
                    let idx = col * n_rows + row;
                    if idx >= candidates.len() {
                        break;
                    }
                    let name = &candidates[idx].name;
                    out.push_str(name);
                    if col != n_cols - 1 {
                        let padding = max_width - name.len();
                        for _ in 0..padding {
                            out.push(' ');
                        }
                    }
                }
                out.push('\n');
            }

            print_output(out.into());
        })
    });
    editor.set_helper(Some(helper));

    // Up/Down: empty input → unconditional navigation; non-empty → prefix search.
    editor.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::NONE),
        EventHandler::Conditional(Box::new(HistNav::new(
            completion_state.clone(),
            Cmd::HistorySearchBackward,
        ))),
    );
    editor.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::NONE),
        EventHandler::Conditional(Box::new(HistNav::new(
            completion_state.clone(),
            Cmd::HistorySearchForward,
        ))),
    );

    // Set up history file
    let history_path = dirs::data_dir().map(|d| d.join("aike").join("repl_history.txt"));
    if let Some(ref path) = history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.load_history(path);
    }

    //
    // Spawn readline thread
    //
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<InputCommand>(64);
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();

    let readline_handle = {
        let input_tx = input_tx.clone();
        let mut editor = editor;
        let prompt = AikePrompt {
            is_multiline: false,
        };
        let history_path = history_path.clone();

        std::thread::spawn(move || {
            println!("Wake up, Aike. Type .help to show hints.\n");

            loop {
                IN_READLINE.store(true, Ordering::Release);
                let readline_result = editor.readline(&prompt);
                IN_READLINE.store(false, Ordering::Release);

                match readline_result {
                    Ok(line) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        match trimmed {
                            ".exit" => {
                                if let Some(ref path) = history_path {
                                    let _ = editor.save_history(path);
                                }
                                let _ = input_tx.blocking_send(InputCommand::Quit);
                                break;
                            }
                            _ => {
                                let _ =
                                    input_tx.blocking_send(InputCommand::Eval(trimmed.to_string()));

                                // Manual global dedup: only add if not already in history.
                                if !trimmed.starts_with('.')
                                    && !history_contains(editor.history(), trimmed)
                                    && let Ok(true) = editor.add_history_entry(trimmed)
                                    && let Some(ref path) = history_path
                                {
                                    let _ = editor.save_history(path);
                                }

                                // Wait for JS thread to finish eval (ACK).
                                let _ = ack_rx.recv();
                            }
                        }
                    }
                    Err(ReadlineError::Interrupted) => {
                        // Ctrl-C
                        continue;
                    }
                    Err(ReadlineError::Eof) => {
                        // Ctrl-D: quit
                        if let Some(ref path) = history_path {
                            let _ = editor.save_history(path);
                        }
                        let _ = input_tx.blocking_send(InputCommand::Quit);
                        break;
                    }
                    Err(err) => {
                        eprintln!("Readline error: {:?}", err);
                        break;
                    }
                }
            }
        })
    };

    //
    // Main JS event loop
    //
    let mut req_rx = completion_state.req_rx.lock().await;

    let exit_code = loop {
        tokio::select! {
            // Drive the runtime tick.
            status = rt.run_async() => {
                if let ExitStatus::Interrupted(code) = status {
                    break ExitCode::from(code);
                }
            }

            // read input line
            cmd = input_rx.recv() => {
                let Some(cmd) = cmd else { break ExitCode::SUCCESS; };
                match cmd {
                    InputCommand::Quit =>{ break ExitCode::SUCCESS; },
                    InputCommand::Eval(line) => {
                        process_input_line(&mut ctx0, &line, &ack_tx);
                    }
                }
            }

            // completion request
            req = req_rx.recv() => {
                let Some(req) = req else { continue };
                ctx0.root_scope().with_new_scope(|cx| {
                    js_compute_completions(cx, req);
                });
            }
        }
    };

    // Readline thread is blocked on editor.readline() and can't be cleanly
    // interrupted from here. The process is exiting anyway, so detach the
    // handle — OS cleanup will terminate the thread.
    readline_handle.thread().unpark();

    exit_code
}

const HELP: &str = "\
\x1b[33m.help\x1b[0m    Show this help message
\x1b[33m.exit\x1b[0m    Exit the REPL
\x1b[33mTab\x1b[0m      Trigger completion
\x1b[33mUp/Down\x1b[0m  Navigate history (search by current input prefix)
\x1b[33mCtrl-C\x1b[0m   Cancel current input
\x1b[33mCtrl-D\x1b[0m   Exit the REPL";
