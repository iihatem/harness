use std::{
    io::{IsTerminal, Read, Write},
    path::Path,
    sync::Arc,
    time::Duration,
};

use harness_config::config;
use harness_core::{
    agent::NonInteractive,
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    redact::{EventRedactor, Redactor},
};
use harness_providers::{credentials::Credentials, registry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    models,
    notices::Notices,
    setup,
    start::{self, Request, Started, end_run},
    term::{terminal_safe, terminal_safe_text},
};

pub async fn run(
    model_flag: Option<String>,
    mode_flag: Option<Mode>,
    session: crate::sessions::Choice,
    prompt_text: String,
    json: bool,
    debug: bool,
) -> u8 {
    // Registered as the very first thing this function does (a plain synchronous call, not an
    // awaited future): once it returns, the OS delivers SIGINT to tokio's signal driver instead of
    // the default disposition, even before the `sigint.recv()` task below gets its first poll.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("failed to install a SIGINT handler");
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    // What is printed before the agent starts, for the debug log.
    let mut notices = Notices::new(setup.redactor.clone());
    for warning in &setup.config.warnings {
        notices.printed(warning);
    }
    let commands = crate::slash::discover(&setup, &prompt_text, &mut notices);
    if let Err(message) = crate::slash::check(&prompt_text, commands.as_ref()) {
        eprintln!("error: {}", terminal_safe(&message));
        return 2;
    }
    let session = match crate::sessions::open(&setup, &session, &mut notices) {
        Ok(session) => session,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let Some(model_id) = crate::start::configured_model(&setup, model_flag) else {
        eprintln!("error: no model configured.");
        let found = models::available(&setup).await;
        credential_warnings(&setup, &mut notices);
        if found.is_empty() {
            eprintln!(
                "No local model servers were found. Start Ollama, LM Studio, or llama.cpp, or configure a provider."
            );
        } else {
            eprintln!("Available models:");
            for model in &found {
                eprintln!("  {}", terminal_safe(&model.id()));
            }
        }
        eprintln!(
            "Pass one with --model, or set `model = \"<provider>/<model>\"` in {}.",
            setup.paths.global_config_file().display()
        );
        return 2;
    };
    let resolved = registry::resolve(&model_id, &setup.config.providers, setup.keys());
    credential_warnings(&setup, &mut notices);
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };

    // Ctrl+C must be honoured from here on: the piped-stdin wait below can block for seconds (the
    // first-data timeout) or indefinitely (the unbounded read-to-EOF phase once data has started
    // arriving but the pipe never closes, e.g. `tail -f | harness ask ...`). So the cancellation
    // token and its SIGINT listener are wired up before that wait, not after it.
    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if sigint.recv().await.is_some() {
            on_ctrl_c.cancel();
        }
    });

    // Only read (and potentially block on) stdin once we know we're actually going to run: a
    // missing model must exit 2 promptly even if a pipe into stdin is still open.
    let typed = prompt_text.clone();
    let input = match with_piped_stdin(prompt_text, cancel.clone(), &mut notices).await {
        StdinOutcome::Ready(input) => input,
        // Cancelled while waiting on stdin: exit immediately, before any model call.
        StdinOutcome::Cancelled => return exit_code(TurnEndReason::Interrupted, false),
    };

    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| config::default_mode(&setup.workspace));
    let run_id = start::run_id();
    let log = if debug {
        match open_log(&setup.paths.state_dir, &run_id) {
            Ok((file, path)) => {
                eprintln!("debug log: {}", terminal_safe(&path.display().to_string()));
                Some(file)
            }
            Err(e) => {
                notices.warn(&format!("cannot write the debug log: {e}"));
                None
            }
        }
    } else {
        None
    };
    let Some(Started {
        mut agent,
        sandbox_session,
        policy,
        ..
    }) = start::start(
        Request {
            setup: &setup,
            mode,
            model: resolved,
            session,
            approver: Arc::new(NonInteractive),
            interactive: false,
            run_id,
            cancel: cancel.clone(),
        },
        &mut notices,
    )
    .await
    else {
        return exit_code(TurnEndReason::Interrupted, false);
    };
    // `with_piped_stdin` returns the prompt with any piped text appended.
    let turn = crate::slash::turn_input(
        &typed,
        &input[typed.len()..],
        commands.as_ref(),
        &setup,
        &*policy,
    );
    crate::slash::print_messages(&turn.messages, &mut notices);
    let turn = turn.input;

    let (tx, rx) = mpsc::unbounded_channel();
    let rx = with_credential_warnings(rx, setup.credentials.clone());
    let renderer = tokio::spawn(render(
        rx,
        json,
        cancel.clone(),
        setup.redactor.clone(),
        log,
        notices.into_events(),
    ));
    let reason = agent.run_turn(turn, &tx, cancel).await;
    drop(tx);
    let (final_text, blocked) = renderer.await.unwrap_or_default();
    if !json && !final_text.is_empty() {
        // A closed stdout pipe must not panic (and so must not lose `blocked`/the exit code). The
        // model's answer can contain prompt-injected ANSI/OSC escapes or bidi overrides, so it is
        // escaped before printing — `terminal_safe_text` keeps `\n`/`\t` so a normal multi-line
        // answer still prints as multiple lines.
        let _ = writeln!(
            std::io::stdout().lock(),
            "{}",
            terminal_safe_text(&final_text)
        );
    }
    agent.close().await;
    end_run(agent, sandbox_session);
    exit_code(reason, blocked)
}

/// Prints what the credential store has had to warn about so far, and keeps it for the debug
/// log.
fn credential_warnings(setup: &setup::Setup, notices: &mut Notices) {
    for warning in setup.credentials.take_warnings() {
        notices.warn(&warning);
    }
}

/// Maps how the turn ended to the documented exit codes.
pub fn exit_code(reason: TurnEndReason, blocked: bool) -> u8 {
    match reason {
        TurnEndReason::Completed if blocked => 3,
        TurnEndReason::Completed => 0,
        TurnEndReason::Interrupted => 130,
        TurnEndReason::Budget => 4,
        TurnEndReason::StepLimit | TurnEndReason::Error => 1,
        TurnEndReason::GateFailed => 5,
    }
}

/// How long to wait for the first byte of data (or EOF) on a piped stdin before giving up on it and
/// running the turn with the prompt alone. Test/automation-only override: `HARNESS_STDIN_WAIT_MS`
/// (milliseconds) — not a documented user-facing setting.
const STDIN_FIRST_DATA_TIMEOUT_MS: u64 = 3000;

fn stdin_wait_timeout() -> Duration {
    std::env::var("HARNESS_STDIN_WAIT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(STDIN_FIRST_DATA_TIMEOUT_MS))
}

/// The result of waiting on piped stdin: either the (possibly stdin-augmented) prompt, or a signal
/// that Ctrl+C arrived while still waiting, in which case the caller must not proceed to a model call.
enum StdinOutcome {
    Ready(String),
    Cancelled,
}

/// Appends piped stdin to `prompt_text`, without blocking the async runtime and without hanging
/// forever on a pipe that a parent process leaves open but never writes to or closes.
///
/// A TTY stdin is never read (unchanged behaviour). Otherwise the blocking read happens on a
/// dedicated thread; this function waits only for that thread's first byte-or-EOF signal, bounded by
/// `stdin_wait_timeout()`. If that first signal doesn't arrive in time, it prints a one-time warning
/// and proceeds with the prompt alone — the reader thread is left running (it may still be blocked in
/// the OS read call), which is fine because the process exits normally when the turn finishes. Once
/// the first signal does arrive, the rest of stdin is read to EOF with no further timeout, so slow
/// producers are still included in full.
///
/// Both the first-data wait and the (potentially unbounded) read-to-EOF join race against `cancel`:
/// Ctrl+C during either phase abandons the reader thread and returns `StdinOutcome::Cancelled`
/// immediately, so the caller can exit without ever making a model call.
async fn with_piped_stdin(
    prompt_text: String,
    cancel: CancellationToken,
    notices: &mut Notices,
) -> StdinOutcome {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return StdinOutcome::Ready(prompt_text);
    }

    // Two one-shot signals from the reader thread: `first_tx` fires the moment the first `read()`
    // call returns (data or immediate EOF), `done_tx` fires once the thread has read to EOF (or hit
    // an error) and has the full buffer. Deliberately not joined via `tokio::task::spawn_blocking`:
    // if this function is cancelled while awaiting `done_rx`, the receiver is simply dropped and the
    // raw OS thread (untracked by Tokio) is left running detached. Wrapping the join in
    // `spawn_blocking` instead would register it as a Tokio-managed blocking task, which the runtime
    // waits for on shutdown — so an abandoned one would hang process exit even after "cancelling".
    let (first_tx, first_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut first_tx = Some(first_tx);
        loop {
            match lock.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(tx) = first_tx.take() {
                        let _ = tx.send(());
                    }
                }
                Err(_) => break,
            }
        }
        // Reached on immediate EOF (e.g. `< /dev/null`) or a read error: still counts as "first
        // signal" so the waiter below doesn't sit out the full timeout for nothing.
        if let Some(tx) = first_tx.take() {
            let _ = tx.send(());
        }
        let _ = done_tx.send(String::from_utf8_lossy(&buf).into_owned());
    });

    let first_signal_received = tokio::select! {
        result = tokio::time::timeout(stdin_wait_timeout(), first_rx) => {
            matches!(result, Ok(Ok(())))
        }
        _ = cancel.cancelled() => return StdinOutcome::Cancelled,
    };
    if !first_signal_received {
        notices.warn(
            "no stdin data received in 3s, proceeding without it (redirect stdin from /dev/null to skip the wait)",
        );
        return StdinOutcome::Ready(prompt_text);
    }

    let piped = tokio::select! {
        result = done_rx => result.unwrap_or_default(),
        _ = cancel.cancelled() => return StdinOutcome::Cancelled,
    };
    if piped.trim().is_empty() {
        StdinOutcome::Ready(prompt_text)
    } else {
        StdinOutcome::Ready(format!("{prompt_text}\n\n{piped}"))
    }
}

/// How often what the credential store warns about is looked for while no event comes.
const CREDENTIAL_WARNINGS_EVERY: Duration = Duration::from_millis(250);

/// Passes `events` on, each after what the credential store has had to warn about by then, as
/// warning events: a sign-in renewed during the turn that could not be stored, say, is told
/// before what the provider sent after the renewal. While no event comes, what it warns about is
/// passed on within [`CREDENTIAL_WARNINGS_EVERY`]: a renewal waiting for another process's says
/// so while it waits.
fn with_credential_warnings(
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    credentials: Arc<Credentials>,
) -> mpsc::UnboundedReceiver<AgentEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let warnings = |tx: &mpsc::UnboundedSender<AgentEvent>| {
            for message in credentials.take_warnings() {
                let _ = tx.send(AgentEvent::Warning { message });
            }
        };
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break };
                    warnings(&tx);
                    let _ = tx.send(event);
                }
                _ = tokio::time::sleep(CREDENTIAL_WARNINGS_EVERY) => warnings(&tx),
            }
        }
        warnings(&tx);
    });
    rx
}

/// Opens `<state>/logs/<run_id>.log` for `--debug`, readable only by its owner, in a directory
/// only its owner can read, even when it was there already.
fn open_log(
    state_dir: &Path,
    run_id: &str,
) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let dir = state_dir.join("logs");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    if std::fs::metadata(&dir)?.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = dir.join(format!("{run_id}.log"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    Ok((file, path))
}

/// Prints events as they arrive, and writes them to the debug `log`, with every secret
/// `redactor` knows replaced, including one the model streams in pieces. The log starts with
/// `startup`, the warnings printed before the agent started. Returns the last assistant text and
/// whether an action was blocked.
///
/// If stdout is closed (e.g. the reader end of a pipe exits early), writing must not panic: it sets
/// `stdout_broken` and cancels the run so it stops promptly, but keeps draining events (so `blocked`
/// is still tracked correctly) until the channel closes.
async fn render(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    json: bool,
    cancel: CancellationToken,
    redactor: Arc<Redactor>,
    mut log: Option<std::fs::File>,
    startup: Vec<AgentEvent>,
) -> (String, bool) {
    if let Some(file) = log.as_mut() {
        for event in &startup {
            let _ = writeln!(
                file,
                "{}",
                serde_json::to_string(event).expect("events serialize")
            );
        }
    }
    let mut events = EventRedactor::new(redactor.clone());
    let mut shown = Shown {
        json,
        cancel,
        redactor,
        log,
        last_text: String::new(),
        blocked: false,
        writes: std::collections::HashMap::new(),
        stdout_broken: false,
    };
    while let Some(event) = rx.recv().await {
        for event in events.push(event) {
            shown.show(event);
        }
    }
    for event in events.finish() {
        shown.show(event);
    }
    (shown.last_text, shown.blocked)
}

/// What [`render`] has shown so far, and where it shows events.
struct Shown {
    json: bool,
    cancel: CancellationToken,
    redactor: Arc<Redactor>,
    log: Option<std::fs::File>,
    last_text: String,
    blocked: bool,
    /// What each `write` or `edit` call would change, to show it when the call is blocked.
    writes: std::collections::HashMap<String, String>,
    stdout_broken: bool,
}

impl Shown {
    /// Shows `event`, already redacted: as a line of NDJSON and of the log, or on the terminal.
    fn show(&mut self, event: AgentEvent) {
        let json = self.json;
        let line = serde_json::to_string(&event).expect("events serialize");
        if let Some(file) = self.log.as_mut() {
            let _ = writeln!(file, "{line}");
        }
        if json && !self.stdout_broken && writeln!(std::io::stdout().lock(), "{line}").is_err() {
            self.stdout_broken = true;
            self.cancel.cancel();
        }
        match &event {
            AgentEvent::AssistantMessage { content, .. } if !content.is_empty() => {
                self.last_text = content.clone()
            }
            AgentEvent::ActionBlocked { id, reason } => {
                self.blocked = true;
                if !json {
                    eprintln!("blocked: {}", terminal_safe(reason));
                    if let Some(proposed) = self.writes.get(id) {
                        eprintln!("{proposed}");
                    }
                }
            }
            AgentEvent::ToolCallRequested {
                id,
                name,
                arguments,
            } if !json => {
                let shown: String = arguments.chars().take(120).collect();
                eprintln!("-> {} {}", terminal_safe(name), terminal_safe(&shown));
                if let Some(proposed) = proposed_change(name, arguments, &self.redactor) {
                    self.writes.insert(id.clone(), proposed);
                }
            }
            AgentEvent::Retrying {
                attempt,
                reason,
                delay_ms,
            } if !json => {
                eprintln!(
                    "retrying (attempt {attempt}) in {delay_ms} ms: {}",
                    terminal_safe(reason)
                );
            }
            AgentEvent::GateResult {
                gate,
                command,
                status,
                exit_code,
                tail,
            } if !json => {
                let (line, _) = harness_tui::transcript::gate_line(
                    *gate,
                    command.as_deref(),
                    *status,
                    *exit_code,
                );
                eprintln!("{}", terminal_safe(&line));
                if let Some(tail) = tail {
                    eprintln!("{}", terminal_safe_text(tail.trim_end()));
                }
            }
            AgentEvent::Error { message, .. } if !json => {
                eprintln!("error: {}", terminal_safe(message))
            }
            AgentEvent::Warning { message } if !json => {
                eprintln!("warning: {}", terminal_safe(message))
            }
            AgentEvent::BudgetWarning { notice } if !json => {
                eprintln!("warning: {}", terminal_safe(&notice.warning_message()))
            }
            AgentEvent::BudgetReached { notice } if !json => {
                eprintln!("error: {}", terminal_safe(&notice.reached_message()))
            }
            AgentEvent::Compacted {
                summary,
                tokens_before,
                tokens_after,
            } if !json => {
                eprintln!(
                    "compacted the conversation from about {tokens_before} to {tokens_after} tokens; summary:\n{}",
                    terminal_safe_text(summary)
                );
            }
            AgentEvent::TurnFinished {
                reason: TurnEndReason::StepLimit,
            } if !json => {
                eprintln!("error: stopped after reaching the step limit");
            }
            AgentEvent::TurnFinished {
                reason: TurnEndReason::GateFailed,
            } if !json => {
                eprintln!("error: stopped because the tests still fail");
            }
            _ => {}
        }
    }
}

/// What a `write` or `edit` call with `arguments` would change, as printed when it is blocked: the
/// whole new content, or the text an edit replaces and its replacement. Only what the model sent
/// is shown; the file itself is not read, since a blocked file may hold secrets. `arguments` come
/// redacted, but the model may have escaped a secret in them otherwise than JSON usually does,
/// so each value is redacted again once decoded.
fn proposed_change(name: &str, arguments: &str, redactor: &Redactor) -> Option<String> {
    let args = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let text = |field: &str| args[field].as_str().map(|text| redactor.redact(text));
    let path = terminal_safe(&text("path")?);
    match name {
        "write" => Some(format!(
            "proposed content of {path}:\n{}",
            terminal_safe_text(&text("content")?)
        )),
        "edit" => {
            let every = if args["replace_all"].as_bool() == Some(true) {
                " every occurrence of"
            } else {
                ""
            };
            Some(format!(
                "proposed edit of {path}, replacing:{every}\n{}\nwith:\n{}",
                terminal_safe_text(&text("old_string")?),
                terminal_safe_text(&text("new_string")?)
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Re-review B+C, R5: a renewal waiting for another process says so while it waits, before any
    // event follows.
    #[tokio::test]
    async fn credential_warnings_are_passed_on_while_no_event_comes() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(Credentials::with_keychain(dir.path(), None));
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = with_credential_warnings(rx, credentials.clone());
        credentials.warn("waiting for another harness process".into());
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("passed on while no event comes");
        assert_eq!(
            event,
            Some(AgentEvent::Warning {
                message: "waiting for another harness process".into()
            })
        );
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    // Review F M6: a logs directory left readable by others is made private again.
    #[test]
    fn the_debug_log_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("logs")).unwrap();
        std::fs::set_permissions(
            state.path().join("logs"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let (_file, path) = open_log(state.path(), "run-1").unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&state.path().join("logs")), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(exit_code(TurnEndReason::Completed, false), 0);
        assert_eq!(exit_code(TurnEndReason::Completed, true), 3);
        assert_eq!(exit_code(TurnEndReason::Error, false), 1);
        assert_eq!(exit_code(TurnEndReason::StepLimit, false), 1);
        assert_eq!(exit_code(TurnEndReason::GateFailed, false), 5);
        assert_eq!(exit_code(TurnEndReason::Interrupted, false), 130);
    }
}
