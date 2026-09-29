//! `harness` without a subcommand: the interactive session in the terminal.

use std::{io::IsTerminal, sync::Arc};

use crossterm::event::EventStream;
use harness_config::config;
use harness_context::{commands::Commands, project::project_root};
use harness_core::{engine::PermissionEngine, permission::Mode};
use harness_providers::registry;
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
    ui::Ui,
};
use ratatui::backend::CrosstermBackend;
use tokio_util::sync::CancellationToken;

use crate::{
    context::home,
    notices::Notices,
    sessions, setup,
    setup::Setup,
    slash::{self, Message},
    start::{self, Request, Started},
    term::terminal_safe,
};

/// Why the interactive session cannot start, when stdin or stdout is not a terminal.
pub fn needs_terminal(stdin: bool, stdout: bool) -> Option<&'static str> {
    (!stdin || !stdout).then_some(
        "interactive mode needs a terminal; use `harness ask \"<prompt>\"` to run a prompt without one",
    )
}

/// Expands the project's custom commands and `/init` for the session.
struct CliHost {
    setup: Arc<Setup>,
    commands: Commands,
    policy: Arc<PermissionEngine>,
}

impl Host for CliHost {
    fn is_command(&self, name: &str) -> bool {
        self.commands.get(name).is_some()
    }

    fn prepare(&mut self, typed: &str) -> Prepared {
        let expanded =
            slash::turn_input(typed, "", Some(&self.commands), &self.setup, &*self.policy);
        let mut prepared = Prepared {
            input: expanded.input,
            notes: Vec::new(),
            warnings: Vec::new(),
        };
        for message in expanded.messages {
            match message {
                Message::Warning(text) => prepared.warnings.push(text),
                Message::Note(text) => prepared.notes.push(text),
            }
        }
        prepared
    }
}

/// Runs the interactive session; the exit code.
pub async fn run(
    model_flag: Option<String>,
    mode_flag: Option<Mode>,
    choice: sessions::Choice,
) -> u8 {
    if let Some(message) = needs_terminal(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        eprintln!("error: {message}");
        return 2;
    }
    let setup = match setup::load() {
        Ok(setup) => Arc::new(setup),
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    // What is printed before the terminal UI starts.
    let mut notices = Notices::new(setup.redactor.clone());
    let session = match sessions::open(&setup, &choice, &mut notices) {
        Ok(session) => session,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let Some(model_id) = model_flag.or_else(|| setup.config.model.clone()) else {
        eprintln!(
            "error: no model configured; pass one with --model, or set `model = \"<provider>/<model>\"` in {}; `harness models` lists the models harness finds",
            setup.paths.global_config_file().display()
        );
        return 2;
    };
    let resolved = registry::resolve(&model_id, &setup.config.providers, setup.keys());
    setup.print_credential_warnings();
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };
    let model = resolved.id.clone();
    let mode = mode_flag
        .or(setup.config.mode)
        .unwrap_or_else(|| config::default_mode(&setup.workspace));
    let commands = harness_context::commands::discover(
        &project_root(&setup.workspace),
        &setup.paths.config_dir,
        home().as_deref(),
    );
    for warning in &commands.warnings {
        notices.warn(warning);
    }
    let (approver, approvals) = ChannelApprover::new();
    // Nothing cancels the start: Ctrl+C before the terminal UI ends the process.
    let Some(Started {
        agent,
        sandbox_session,
        policy,
    }) = start::start(
        Request {
            setup: &setup,
            mode,
            model: resolved,
            session,
            approver,
            interactive: true,
            run_id: start::run_id(),
            cancel: CancellationToken::new(),
        },
        &mut notices,
    )
    .await
    else {
        return 130;
    };
    let history = agent.rewind_points().into_iter().map(|p| p.text).collect();
    let options = Options {
        theme: Theme::from_env(),
        model,
        mode,
        commands: commands.listing(),
        workspace: setup.workspace.clone(),
        history,
        instruction_files: crate::context::instruction_files(&setup),
        window_note: Some("assumed until model profiles report the model's own".into()),
    };
    let host = CliHost {
        setup: setup.clone(),
        commands,
        policy,
    };
    let result = terminal_session(agent, Box::new(host), options, approvals).await;
    sandbox_session.end();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}

/// Runs the session on this process's terminal, and gives the terminal back as it was.
async fn terminal_session(
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    options: Options,
    approvals: Requests,
) -> std::io::Result<()> {
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let (column, row) = crossterm::cursor::position().unwrap_or((0, 0));
    let top = if column == 0 { row } else { row + 1 };
    let _modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let ui = Ui::start(agent, host, term, options, approvals);
    ui.run(EventStream::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_terminal_it_names_harness_ask() {
        assert!(needs_terminal(true, true).is_none());
        for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
            let message = needs_terminal(stdin, stdout).unwrap();
            assert!(message.contains("harness ask"), "{message}");
        }
    }
}
