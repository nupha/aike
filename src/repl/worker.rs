// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::sync::{Arc, Mutex};

use rustyline::{
    Editor, ExternalPrinter as _, Prompt, config::Builder as ConfigBuilder, config::CompletionType,
    error::ReadlineError, history::FileHistory,
};

use crate::repl::{
    completion::{AikeHelper, CompletionState, js_compute_completions},
    prompt::AikePrompt,
};

/// Commands sent from the readline thread to the JS thread.
#[derive(Debug)]
pub(crate) enum InputCommand {
    Eval(String),
    Quit,
}

/// Start the REPL event loop.
pub(crate) async fn run<'c>(rt: &mut zuqe::Runtime, ctx: &mut zuqe::Context<'c>) {
    let completion_state = CompletionState::new();

    // Build rustyline editor
    let config = ConfigBuilder::new()
        .auto_add_history(false)
        .completion_type(CompletionType::List)
        .build();

    let mut editor =
        Editor::<AikeHelper, FileHistory>::with_config(config).expect("Editor::with_config");

    // Create ExternalPrinter first so it can be shared with the helper
    let printer = editor
        .create_external_printer()
        .expect("create_external_printer");
    let printer = Arc::new(Mutex::new(printer));

    let mut helper = AikeHelper::new(completion_state.clone());
    let printer_for_candidates = printer.clone();
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

            if let Ok(mut p) = printer_for_candidates.lock() {
                let _ = p.print(out);
            }
        })
    });
    editor.set_helper(Some(helper));

    // Set up history file
    let history_path = dirs::data_dir().map(|d| d.join("aike").join("repl_history.txt"));
    if let Some(ref path) = history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.load_history(path);
    }

    // Redirect console.log output through ExternalPrinter
    crate::global::set_console_output_hook({
        let printer = printer.clone();
        Box::new(move |msg| {
            if let Ok(mut p) = printer.lock() {
                let _ = p.print(msg);
            }
        })
    });

    //
    // Spawn readline thread
    //
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<InputCommand>(64);

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
                match editor.readline(prompt.raw()) {
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

                                // Meta-commands (.exit, .help) should not appear in history.
                                if !trimmed.starts_with('.')
                                    && let Ok(true) = editor.add_history_entry(trimmed)
                                    && let Some(ref path) = history_path
                                {
                                    let _ = editor.save_history(path);
                                }
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

    loop {
        tokio::select! {
            // Drive the runtime tick.
            _ =  rt.run_async() => {}

            // read input line
            cmd = input_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    InputCommand::Quit =>{ break; },
                    InputCommand::Eval(line) => {
                        match line.as_str() {
                            ".help" => {
                                if let Ok(mut p) = printer.lock() {
                                    let _ = p.print(HELP.to_string());
                                }
                                continue;
                            }
                            _ => {
                                let mut outputs: Vec<String> = Vec::new();
                                let mut eval_error: Option<String> = None;

                                ctx.with_new_scope(|mut scope| {
                                    match scope.eval(
                                        line.as_bytes(),
                                        "<repl>",
                                        zuqe::EvalType::Global,
                                        zuqe::EvalFlags::empty(),
                                    ) {
                                        Ok(val) => {
                                            let result = val
                                                .inspect(&mut scope)
                                                .unwrap_or_else(|_| "<error>".to_string());
                                            if !result.is_empty() && result != "undefined" {
                                                outputs.push(result);
                                            }
                                        }
                                        Err(e) => {
                                            eval_error = Some(e.to_string());
                                        }
                                    }

                                    scope.return_value(())
                                });

                                if let Ok(mut p) = printer.lock() {
                                    for msg in outputs {
                                        let _ = p.print(msg);
                                    }
                                    if let Some(err) = eval_error {
                                        let _ = p.print(format!("\x1b[31m{}\x1b[0m", err));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // completion request
            req = req_rx.recv() => {
                let Some(req) = req else { continue };
                ctx.with_new_scope(|mut scope| {
                    js_compute_completions(&mut scope, req);
                    scope.return_value(())
                });
            }
        }
    }

    let _ = readline_handle.join();
}

const HELP: &str = "\
\x1b[33m.help\x1b[0m    Show this help message
\x1b[33m.exit\x1b[0m    Exit the REPL
\x1b[33mTab\x1b[0m      Trigger completion
\x1b[33mCtrl-C\x1b[0m   Cancel current input
\x1b[33mCtrl-D\x1b[0m   Exit the REPL";
