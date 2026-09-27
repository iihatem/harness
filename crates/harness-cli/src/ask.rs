use std::{
    io::{IsTerminal, Read, Write},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use harness_config::config;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine, RuleSet},
    event::{AgentEvent, TurnEndReason},
    permission::Mode,
    tool::ToolContext,
};
use harness_providers::registry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{models, prompt, setup};

pub async fn run(
    model_flag: Option<String>,
    mode_flag: Option<Mode>,
    prompt_text: String,
    json: bool,
) -> u8 {
    // Registered as the very first thing this function does (a plain synchronous call, not an
    // awaited future): once it returns, the OS delivers SIGINT to tokio's signal driver instead of
    // the default disposition, even before the `sigint.recv()` task below gets its first poll.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("failed to install a SIGINT handler");
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {message}");
            return 2;
        }
    };
    let Some(model_id) = model_flag.or_else(|| setup.config.model.clone()) else {
        eprintln!("error: no model configured.");
        let found = models::available(&setup).await;
        if found.is_empty() {
            eprintln!(
                "No local model servers were found. Start Ollama, LM Studio, or llama.cpp, or configure a provider."
            );
        } else {
            eprintln!("Available models:");
            for model in &found {
                eprintln!("  {}", model.id());
            }
        }
        eprintln!(
            "Pass one with --model, or set `model = \"<provider>/<model>\"` in {}.",
            setup.paths.global_config_file().display()
        );
        return 2;
    };
    let resolved = match registry::resolve(&model_id, &setup.config.providers, setup::env) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {e}");
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
    let input = match with_piped_stdin(prompt_text, cancel.clone()).await {
        StdinOutcome::Ready(input) => input,
        // Cancelled while waiting on stdin: exit immediately, before any model call.
        StdinOutcome::Cancelled => return exit_code(TurnEndReason::Interrupted, false),
    };

    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| config::default_mode(&setup.workspace));
    if mode == Mode::FullAccess {
        eprintln!("warning: full-access mode: commands run without approval or sandbox");
    }
    let run_id = format!(
        "run-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::process::id()
    );
    let output_dir = setup.paths.state_dir.join("tool-output").join(run_id);
    let sandbox_disabled_by_env =
        std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none") && mode != Mode::FullAccess;
    let sandbox = if mode == Mode::FullAccess || sandbox_disabled_by_env {
        None
    } else {
        harness_sandbox::detect(harness_sandbox::SandboxSettings {
            extra_writable: setup.config.writable_roots.clone(),
            allow_localhost: setup.config.allow_localhost,
        })
    };
    let sandboxed = sandbox.is_some();
    if mode != Mode::FullAccess && !sandboxed {
        if sandbox_disabled_by_env {
            eprintln!(
                "warning: the sandbox is disabled by HARNESS_SANDBOX=none; every shell command will need approval"
            );
        } else {
            eprintln!(
                "warning: no OS sandbox is available; every shell command will need approval"
            );
        }
    }
    let mut read_dirs = setup.config.read_dirs.clone();
    read_dirs.push(output_dir.clone());
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode,
        workspace: setup.workspace.clone(),
        read_dirs,
        rules: RuleSet {
            allow: setup.config.allow.clone(),
            deny: setup.config.deny.clone(),
            confirm: setup.config.confirm.clone(),
        },
        sandbox_available: sandboxed,
    }));
    for rule in policy.unknown_rules() {
        eprintln!("warning: rule `{rule}` names an unknown tool (use bash:, read:, or write:)");
    }
    let ctx = ToolContext::new(&setup.workspace).with_sandbox(sandbox, mode.fs_access());
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        prompt::system_prompt(&setup.workspace, &prompt::today_utc(), mode, sandboxed),
        output_dir,
    );
    if let Some(steps) = setup.config.max_steps {
        config.max_steps = steps;
    }
    let mut agent = Agent::new(
        resolved.provider,
        harness_tools::builtin(),
        policy,
        Arc::new(NonInteractive),
        config,
        ctx,
    );

    let (tx, rx) = mpsc::unbounded_channel();
    let renderer = tokio::spawn(render(rx, json, cancel.clone()));
    let reason = agent.run_turn(input, &tx, cancel).await;
    drop(tx);
    let (final_text, blocked) = renderer.await.unwrap_or_default();
    if !json && !final_text.is_empty() {
        // A closed stdout pipe must not panic (and so must not lose `blocked`/the exit code).
        let _ = writeln!(std::io::stdout().lock(), "{final_text}");
    }
    exit_code(reason, blocked)
}

/// Maps how the turn ended to the documented exit codes.
pub fn exit_code(reason: TurnEndReason, blocked: bool) -> u8 {
    match reason {
        TurnEndReason::Completed if blocked => 3,
        TurnEndReason::Completed => 0,
        TurnEndReason::Interrupted => 130,
        TurnEndReason::StepLimit | TurnEndReason::Error => 1,
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
async fn with_piped_stdin(prompt_text: String, cancel: CancellationToken) -> StdinOutcome {
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
        eprintln!(
            "warning: no stdin data received in 3s, proceeding without it (redirect stdin from /dev/null to skip the wait)"
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

/// Prints events as they arrive. Returns the last assistant text and whether an action was blocked.
///
/// If stdout is closed (e.g. the reader end of a pipe exits early), writing must not panic: it sets
/// `stdout_broken` and cancels the run so it stops promptly, but keeps draining events (so `blocked`
/// is still tracked correctly) until the channel closes.
async fn render(
    mut rx: mpsc::UnboundedReceiver<AgentEvent>,
    json: bool,
    cancel: CancellationToken,
) -> (String, bool) {
    let mut last_text = String::new();
    let mut blocked = false;
    let mut stdout_broken = false;
    while let Some(event) = rx.recv().await {
        if json && !stdout_broken {
            let line = serde_json::to_string(&event).expect("events serialize");
            if writeln!(std::io::stdout().lock(), "{line}").is_err() {
                stdout_broken = true;
                cancel.cancel();
            }
        }
        match &event {
            AgentEvent::AssistantMessage { content, .. } if !content.is_empty() => {
                last_text = content.clone()
            }
            AgentEvent::ActionBlocked { reason, .. } => {
                blocked = true;
                if !json {
                    eprintln!("blocked: {reason}");
                }
            }
            AgentEvent::ToolCallRequested {
                name, arguments, ..
            } if !json => {
                let shown: String = arguments.chars().take(120).collect();
                eprintln!("-> {name} {shown}");
            }
            AgentEvent::Retrying {
                attempt,
                reason,
                delay_ms,
            } if !json => {
                eprintln!("retrying (attempt {attempt}) in {delay_ms} ms: {reason}");
            }
            AgentEvent::Error { message, .. } if !json => eprintln!("error: {message}"),
            AgentEvent::TurnFinished {
                reason: TurnEndReason::StepLimit,
            } if !json => {
                eprintln!("error: stopped after reaching the step limit");
            }
            _ => {}
        }
    }
    (last_text, blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_spec() {
        assert_eq!(exit_code(TurnEndReason::Completed, false), 0);
        assert_eq!(exit_code(TurnEndReason::Completed, true), 3);
        assert_eq!(exit_code(TurnEndReason::Error, false), 1);
        assert_eq!(exit_code(TurnEndReason::StepLimit, false), 1);
        assert_eq!(exit_code(TurnEndReason::Interrupted, false), 130);
    }
}
