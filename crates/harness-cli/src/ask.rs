use std::{
    io::{IsTerminal, Read, Write},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    event::{AgentEvent, TurnEndReason},
    permission::{BaselinePolicy, Mode},
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
    // Only read (and potentially block on) stdin once we know we're actually going to run: a
    // missing model must exit 2 promptly even if a pipe into stdin is still open.
    let input = with_piped_stdin(prompt_text);

    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| setup::default_mode(&setup.workspace));
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
    let policy = Arc::new(BaselinePolicy::new(
        mode,
        &setup.workspace,
        vec![output_dir.clone()],
    ));
    let mut config = AgentConfig::new(
        resolved.id.clone(),
        resolved.model.clone(),
        prompt::system_prompt(&setup.workspace, &prompt::today_utc(), mode),
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
        ToolContext::new(&setup.workspace),
    );

    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if sigint.recv().await.is_some() {
            on_ctrl_c.cancel();
        }
    });

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

fn with_piped_stdin(prompt_text: String) -> String {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return prompt_text;
    }
    let mut piped = String::new();
    if stdin.lock().read_to_string(&mut piped).is_ok() && !piped.trim().is_empty() {
        format!("{prompt_text}\n\n{piped}")
    } else {
        prompt_text
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
