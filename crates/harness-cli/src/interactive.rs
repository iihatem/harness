//! `harness` without a subcommand: the interactive session in the terminal.

use std::{
    future::Future,
    io::{IsTerminal, Write},
    sync::Arc,
};

use harness_config::config;
use harness_context::{commands::Commands, project::project_root};
use harness_core::{engine::PermissionEngine, permission::Mode};
use harness_providers::registry;
use harness_tui::{
    app::{Host, Options, Prepared},
    approval::{ChannelApprover, Requests},
    inline::InlineTerminal,
    input::TerminalInput,
    notify::TerminalNotifier,
    plan::ExternalEditor,
    style::Theme,
    terminal::{CrosstermRawMode, Modes},
    ui::{Ending, Ui},
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

    fn take_warnings(&self) -> Vec<String> {
        self.setup.credentials.take_warnings()
    }

    fn prepare(&mut self, typed: &str) -> Prepared {
        let expanded =
            slash::turn_input(typed, "", Some(&self.commands), &self.setup, &*self.policy);
        let mut prepared = Prepared {
            input: expanded.input,
            notes: Vec::new(),
            warnings: Vec::new(),
        };
        // Redacted, as `harness ask` prints them.
        let redacted = |text: String| self.setup.redactor.redact(&text);
        for message in expanded.messages {
            match message {
                Message::Warning(text) => prepared.warnings.push(redacted(text)),
                Message::Note(text) => prepared.notes.push(redacted(text)),
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
    // Before the configuration loads, so that settings trusted now apply at once.
    if let (Ok(workspace), Ok(paths)) = (
        std::env::current_dir().and_then(|d| d.canonicalize()),
        harness_config::paths::Paths::from_process_env(),
    ) {
        let asked = crate::trust::first_use(
            &workspace,
            &paths,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
        );
        if let Err(e) = asked {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 1;
        }
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
    // Redacted, as `harness ask` prints them; those raised later go to the transcript.
    for warning in setup.credentials.take_warnings() {
        notices.warn(&warning);
    }
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
        window_note,
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
        window_note: Some(window_note),
        // Where Build goes when the session started in plan mode.
        default_mode: setup
            .config
            .mode
            .filter(|m| !matches!(m, Mode::Plan | Mode::ReadOnly))
            .unwrap_or_else(|| config::default_mode(&setup.workspace)),
        text_editor: None,
        notifier: None,
    };
    let host = CliHost {
        setup: setup.clone(),
        commands,
        policy,
    };
    let notifications = setup.config.notifications;
    let redactor = setup.redactor.clone();
    // The session's terminal modes are undone when it returns, before the sandbox's session
    // ends.
    let result = terminal_session(
        agent,
        Box::new(host),
        options,
        approvals,
        notifications,
        redactor,
    )
    .await;
    sandbox_session.end();
    match result {
        Ok(ending) => exit_code(ending),
        Err(e) => {
            // The terminal may be gone.
            let _ = writeln!(
                std::io::stderr(),
                "error: {}",
                terminal_safe(&e.to_string())
            );
            1
        }
    }
}

/// The exit code for how the session ended: 128 plus the signal's number for a hangup (a
/// terminal that closed counts as one) and for SIGTERM, as a shell reports them.
fn exit_code(ending: Ending) -> u8 {
    match ending {
        Ending::Quit => 0,
        Ending::Hangup => 129,
        Ending::Terminated => 143,
    }
}

/// Resolves when harness is asked to stop (SIGTERM) or its terminal hangs up (SIGHUP), so that
/// the session still stops what runs, gives the terminal back, and ends the sandbox's session.
/// Hangups that harness was started ignoring (`nohup`) stay ignored: the session then ends when
/// the terminal's input does.
fn shutdown_signals() -> std::io::Result<impl Future<Output = Ending>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = if ignored(nix::libc::SIGHUP) {
        None
    } else {
        Some(signal(SignalKind::hangup())?)
    };
    Ok(async move {
        let hung_up = async {
            match hangup.as_mut() {
                Some(hangup) => {
                    hangup.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = terminate.recv() => Ending::Terminated,
            () = hung_up => Ending::Hangup,
        }
    })
}

/// Whether this process ignores `signal`.
fn ignored(signal: nix::libc::c_int) -> bool {
    // SAFETY: an all-zero `sigaction` is valid for the call to overwrite; with no new action
    // given, `sigaction` only reads the current one.
    let mut current: nix::libc::sigaction = unsafe { std::mem::zeroed() };
    let read = unsafe { nix::libc::sigaction(signal, std::ptr::null(), &mut current) };
    read == 0 && current.sa_sigaction == nix::libc::SIG_IGN
}

/// Runs the session on this process's terminal, and gives the terminal back as it was.
async fn terminal_session(
    agent: harness_core::agent::Agent,
    host: Box<dyn Host>,
    mut options: Options,
    approvals: Requests,
    notifications: harness_config::config::Notifications,
    redactor: Arc<harness_core::redact::Redactor>,
) -> std::io::Result<Ending> {
    // From here on, a hangup or SIGTERM ends the session rather than harness.
    let shutdown = shutdown_signals()?;
    // Asked before any events are read: both queries read the terminal's answer from stdin.
    let keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    let (column, row) = crossterm::cursor::position().unwrap_or((0, 0));
    let top = if column == 0 { row } else { row + 1 };
    // The editor for plans owns the terminal's modes, so they are undone while it runs, and
    // when the session ends; the terminal is not read for the session meanwhile.
    let modes = Modes::enter(std::io::stdout(), CrosstermRawMode, keyboard)?;
    let input = TerminalInput::start();
    options.text_editor = Some(Box::new(
        ExternalEditor::from_env(modes).pausing(input.pauser()),
    ));
    options.notifier = Some(Box::new(TerminalNotifier::new(
        std::io::stdout(),
        notifications.desktop,
        notifications.bell,
    )));
    let term = InlineTerminal::new(CrosstermBackend::new(std::io::stdout()), top)?;
    let mut ui = Ui::start(agent, host, term, options, approvals).with_redactor(redactor);
    ui.run(input, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use harness_config::{config::Config, paths::Paths, trust::TrustStore};
    use harness_core::{
        engine::{EngineConfig, RuleSet},
        redact::Redactor,
    };
    use harness_providers::credentials::Credentials;

    // What a custom command's expansion says reaches the transcript with the secrets harness
    // knows redacted, as `harness ask` prints it through its notices.
    #[test]
    fn what_a_command_says_is_redacted() {
        const SECRET: &str = "sk-canary-0123456789abcdef";
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(workspace.join(".harness/commands")).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        std::fs::write(
            workspace.join(".harness/commands/deploy.md"),
            format!("---\nmodel: openai/{SECRET}\n---\nDeploy it.\n"),
        )
        .unwrap();
        let home = dir.path().join("home");
        let paths =
            Paths::from_env(|key| (key == "HARNESS_HOME").then(|| home.display().to_string()))
                .unwrap();
        let redactor = Arc::new(Redactor::default());
        redactor.add(SECRET);
        let setup = Arc::new(Setup {
            config: Config::default(),
            workspace: workspace.clone(),
            trust: TrustStore::load(&paths.data_dir).unwrap(),
            credentials: Arc::new(Credentials::with_keychain(&paths.data_dir, None)),
            redactor,
            paths,
        });
        let commands =
            harness_context::commands::discover(&workspace, &setup.paths.config_dir, None);
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
            mode: Mode::Auto,
            workspace: workspace.clone(),
            read_dirs: Vec::new(),
            rules: RuleSet::default(),
            sandbox_available: false,
            writes_need_approval: false,
        }));
        let mut host = CliHost {
            setup,
            commands,
            policy,
        };
        assert!(host.is_command("deploy"));
        let prepared = host.prepare("/deploy");
        let said = [prepared.notes, prepared.warnings].concat().join("\n");
        assert!(said.contains("asks for model openai/[redacted]"), "{said}");
        assert!(!said.contains(&SECRET[8..]), "{said}");
    }

    #[test]
    fn without_a_terminal_it_names_harness_ask() {
        assert!(needs_terminal(true, true).is_none());
        for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
            let message = needs_terminal(stdin, stdout).unwrap();
            assert!(message.contains("harness ask"), "{message}");
        }
    }
}
