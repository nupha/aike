// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use reedline::{
    ColumnarMenu, FileBackedHistory, HistoryItem, KeyCode, KeyModifiers, MenuBuilder, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, default_emacs_keybindings,
};
use zuqe::{
    EvalFlags, EvalType, Exception, Runtime, TopLevelContext, builtins::promise::PromiseState,
};

use super::completion::{
    AikeCompleter, CompletionRequest, CompletionResponse, compute_completions,
};
use super::prompt::AikePrompt;

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
    println!("  Dynamic import() is supported (returns a Promise).");
    println!("  Multi-line input is automatically detected (unbalanced brackets).");
    println!("  Press Ctrl-C to cancel multi-line input.");
    println!("  Press Ctrl-D to exit.");
}

fn add_history(editor: &mut Reedline, line: &str) {
    let item = HistoryItem::from_command_line(line);
    let _ = editor.history_mut().save(item);
}

// ---------------------------------------------------------------------------
// Multi-line input detection
// ---------------------------------------------------------------------------

/// Check whether `code` has balanced brackets/braces and closed strings/comments.
/// Returns `true` when the input is syntactically complete for evaluation.
fn is_input_complete(code: &str) -> bool {
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
// Readline thread command
// ---------------------------------------------------------------------------

/// Commands sent from the main thread to the readline thread.
enum ReadlineCmd {
    SetMultiline(bool),
    AddHistory(String),
    Shutdown,
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Process a single signal from the readline thread.
///
/// Returns `true` to continue the REPL loop, `false` to exit.
fn handle_input(
    sig: Signal,
    multi_line: &mut String,
    last_was_ctrl_c: &mut bool,
    cmd_tx: &std::sync::mpsc::Sender<ReadlineCmd>,
    printer_sender: &crossbeam::channel::Sender<String>,
    rt: &mut Runtime,
    ctx: &mut TopLevelContext<'_>,
    completion_req_rx: &Arc<Mutex<std::sync::mpsc::Receiver<CompletionRequest>>>,
    completion_resp_tx: &std::sync::mpsc::Sender<CompletionResponse>,
) -> bool {
    match sig {
        Signal::CtrlC => {
            multi_line.clear();
            let _ = cmd_tx.send(ReadlineCmd::SetMultiline(false));
            if *last_was_ctrl_c {
                let _ = printer_sender.send("^C".to_string());
                return false;
            }
            *last_was_ctrl_c = true;
            let _ = printer_sender.send("^C(Press Ctrl-C again to quit)".to_string());
            true
        }
        Signal::CtrlD => {
            if !multi_line.is_empty() {
                let _ = printer_sender.send("\x1b[33m(cancelled)\x1b[0m".to_string());
                multi_line.clear();
                let _ = cmd_tx.send(ReadlineCmd::SetMultiline(false));
                return true;
            }
            false
        }
        Signal::Success(line) => {
            *last_was_ctrl_c = false;

            if line.trim().is_empty() && multi_line.is_empty() {
                return true;
            }

            let trimmed = line.trim();
            if multi_line.is_empty() && matches!(trimmed, "/exit" | "/quit" | "/help") {
                match trimmed {
                    "/exit" | "/quit" => return false,
                    "/help" => print_repl_help(),
                    _ => unreachable!(),
                }
                let _ = cmd_tx.send(ReadlineCmd::AddHistory(trimmed.to_string()));
                return true;
            }

            multi_line.push_str(&line);
            multi_line.push('\n');

            if !is_input_complete(multi_line) {
                let _ = cmd_tx.send(ReadlineCmd::SetMultiline(true));
                return true;
            }

            let _ = cmd_tx.send(ReadlineCmd::SetMultiline(false));
            let code = std::mem::take(multi_line);
            let code_trimmed = code.trim();

            if code_trimmed.is_empty() {
                return true;
            }

            let _ = cmd_tx.send(ReadlineCmd::AddHistory(code_trimmed.to_string()));

            let mut eval_error: Option<String> = None;

            ctx.with_new_scope(|mut scope| {
                let r = (|| -> Result<(), i32> {
                    match scope.eval(
                        code_trimmed.as_bytes(),
                        "<repl>",
                        EvalType::Global,
                        EvalFlags::COMPILE_TO_ASYNC,
                    ) {
                        Ok(v) => {
                            if let Some(p) = v.as_object_Promise() {
                                match p.state() {
                                    PromiseState::Fulfilled => {
                                        let result = p.result();
                                        if let Ok(s) = result.inspect(&mut scope) {
                                            let _ = printer_sender.send(s);
                                        }
                                    }
                                    PromiseState::Rejected => {
                                        let reason = p.result();
                                        if let Ok(s) = reason.to_string(&mut scope) {
                                            let _ = printer_sender.send(format!(
                                                "\x1b[31m{}\x1b[0m",
                                                s.encode_utf8(false)
                                            ));
                                        }
                                    }
                                    PromiseState::Pending => {
                                        if let Ok(fut) = (*p).into_future(&mut scope) {
                                            let promise_res = tokio::task::block_in_place(|| {
                                                tokio::runtime::Handle::current().block_on(async {
                                                    tokio::join!(rt.run_async(), fut)
                                                })
                                            })
                                            .1;
                                            match promise_res {
                                                Ok(resolved) => {
                                                    if let Ok(s) = resolved.inspect(&mut scope) {
                                                        let _ = printer_sender.send(s);
                                                    }
                                                }
                                                Err(reason) => {
                                                    if let Ok(s) = reason.to_string(&mut scope) {
                                                        let _ = printer_sender.send(format!(
                                                            "\x1b[31m{}\x1b[0m",
                                                            s.encode_utf8(false),
                                                        ));
                                                    }
                                                }
                                            }
                                        } else {
                                            if let Ok(s) = v.inspect(&mut scope) {
                                                let _ = printer_sender.send(s);
                                            }
                                        }
                                    }
                                }
                            } else {
                                if let Ok(s) = v.inspect(&mut scope) {
                                    let _ = printer_sender.send(s);
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
                let _ = printer_sender.send(format!("\x1b[31m{}\x1b[0m", err));
            }

            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// REPL main loop
// ---------------------------------------------------------------------------

/// Main REPL loop.
///
/// - Uses reedline for line editing, history, and syntax highlighting.
/// - Detects multi-line input via bracket/string balance.
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
    let mut multi_line = String::new();
    let mut last_was_ctrl_c = false;

    println!("\x1b[32mWelcome to Aike REPL! Type /help for info.\x1b[0m");

    // Channel: readline thread -> main thread.
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Signal>(1);

    // Channel: main thread -> readline thread.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ReadlineCmd>();

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();

    // Completion channels: readline thread <-> main thread.
    let (completion_req_tx, completion_req_rx) = std::sync::mpsc::channel::<CompletionRequest>();
    let (completion_resp_tx, completion_resp_rx) = std::sync::mpsc::channel::<CompletionResponse>();

    // Channel: readline thread -> main thread for the ExternalPrinter sender.
    let (printer_sender_tx, printer_sender_rx) =
        std::sync::mpsc::channel::<crossbeam::channel::Sender<String>>();

    // Spawn a dedicated OS thread for reedline's blocking read_line.
    let readline_thread = std::thread::Builder::new()
        .name("readline".into())
        .spawn(move || {
            let completer = Box::new(AikeCompleter {
                req_tx: completion_req_tx,
                resp_rx: completion_resp_rx,
            });
            let completion_menu = Box::new(ColumnarMenu::default().with_name("completion_menu"));

            // Set up keybindings: Tab triggers completion menu.
            let mut keybindings = default_emacs_keybindings();
            keybindings.add_binding(
                KeyModifiers::NONE,
                KeyCode::Tab,
                ReedlineEvent::UntilFound(vec![
                    ReedlineEvent::Menu("completion_menu".to_string()),
                    ReedlineEvent::MenuNext,
                ]),
            );
            let edit_mode = Box::new(reedline::Emacs::new(keybindings));

            // ExternalPrinter allows safe printing while read_line is active.
            let external_printer = reedline::ExternalPrinter::<String>::default();
            let printer_sender = external_printer.sender();
            let _ = printer_sender_tx.send(printer_sender);

            let mut editor = Box::new(
                Reedline::create()
                    .with_history(Box::new(history))
                    .with_highlighter(Box::new(highlighter))
                    .with_completer(completer)
                    .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
                    .with_edit_mode(edit_mode)
                    .with_external_printer(external_printer)
                    .with_quick_completions(true)
                    .with_break_signal(shutdown_flag_clone),
            );
            let mut prompt = AikePrompt {
                is_multiline: false,
            };

            loop {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        ReadlineCmd::SetMultiline(v) => prompt.is_multiline = v,
                        ReadlineCmd::AddHistory(entry) => {
                            add_history(&mut editor, &entry);
                        }
                        ReadlineCmd::Shutdown => {
                            let _ = editor.history_mut().sync();
                            return;
                        }
                    }
                }

                let sig = match editor.read_line(&prompt) {
                    Ok(s) => s,
                    Err(_) => {
                        let _ = input_tx.blocking_send(Signal::CtrlD);
                        return;
                    }
                };

                if matches!(sig, Signal::ExternalBreak(_)) {
                    let _ = editor.history_mut().sync();
                    return;
                }

                if input_tx.blocking_send(sig).is_err() {
                    let _ = editor.history_mut().sync();
                    return;
                }
            }
        })
        .unwrap();

    // Wrap completion request receiver in Arc<Mutex> so we can check it
    // from the select! loop without blocking.
    let completion_req_rx = Arc::new(Mutex::new(completion_req_rx));

    // Receive the ExternalPrinter sender from the readline thread.
    let printer_sender: crossbeam::channel::Sender<String> = printer_sender_rx.recv().unwrap();

    // Redirect console.log (and friends) output through ExternalPrinter so it
    // isn't overwritten by reedline's input line repaint.
    let printer_sender_clone = printer_sender.clone();
    crate::global::set_console_output_hook(Box::new(move |msg| {
        let _ = printer_sender_clone.send(msg);
    }));

    loop {
        // Check for pending completion requests (non-blocking).
        {
            let rx = completion_req_rx.lock().unwrap();
            if let Ok(req) = rx.try_recv() {
                let result = Arc::new(Mutex::new(None::<(Vec<String>, usize)>));
                let result_clone = result.clone();
                ctx.with_new_scope(|mut scope| {
                    let r = compute_completions(&mut scope, &req.line, req.pos);
                    *result_clone.lock().unwrap() = Some(r);
                    scope.return_value(())
                });
                if let Some((suggestions, word_start)) = result.lock().unwrap().take() {
                    let _ = completion_resp_tx.send(CompletionResponse {
                        suggestions,
                        word_start,
                    });
                }
            }
        }

        tokio::select! {
            // Drive the runtime tick.
            _ = rt.run_async() => {}

            // Read input and process.
            Some(sig) = input_rx.recv() => {
                if !handle_input(
                    sig,
                    &mut multi_line,
                    &mut last_was_ctrl_c,
                    &cmd_tx,
                    &printer_sender,
                    rt,
                    ctx,
                    &completion_req_rx,
                    &completion_resp_tx,
                ) {
                    break;
                }
            }
        }
    }

    let _ = cmd_tx.send(ReadlineCmd::Shutdown);
    shutdown_flag.store(true, Ordering::Relaxed);
    drop(input_rx);

    let _ = readline_thread.join();
    println!();
    ExitCode::SUCCESS
}
