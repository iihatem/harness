mod ask;
mod auth;
mod context;
mod doctor;
mod host;
mod interactive;
mod login;
mod models;
mod notices;
mod prompt;
mod sandbox;
mod sessions;
mod setup;
mod slash;
mod start;
mod term;
mod trust;
mod usage;

use std::{io::IsTerminal, process::ExitCode};

use clap::{CommandFactory, Parser, Subcommand};
use harness_core::permission::Mode;

#[derive(Parser)]
#[command(
    name = "harness",
    version,
    about = "A hybrid local/frontier coding agent"
)]
struct Cli {
    /// Model to use, as <provider>/<model> (e.g. ollama/qwen3-coder:30b)
    #[arg(long, global = true)]
    model: Option<String>,
    /// Approval mode: plan, read-only, ask, auto, or full-access
    #[arg(long, global = true)]
    mode: Option<Mode>,
    /// Continue the most recent session in this project
    #[arg(short = 'c', long = "continue", global = true)]
    continue_session: bool,
    /// With `ask`: also write the run's events and warnings, secrets redacted, to a log file in
    /// the state directory
    #[arg(long, global = true)]
    debug: bool,
    /// Resume the session with this id; without an id, list this project's sessions
    #[arg(
        long,
        global = true,
        value_name = "ID",
        num_args = 0..=1,
        conflicts_with = "continue_session"
    )]
    resume: Option<Option<String>>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one prompt to completion without interaction; piped stdin is appended to the prompt
    Ask {
        /// Print every event as one JSON object per line
        #[arg(long)]
        json: bool,
        /// The prompt
        #[arg(required = true, num_args = 1..)]
        prompt: Vec<String>,
    },
    /// List models from local servers and configured providers
    Models,
    /// Store API keys and choose account profiles
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Sign in to ChatGPT, in the browser or with a device code
    Login {
        /// The provider to sign in to: chatgpt
        provider: String,
        /// The account profile to sign in under
        #[arg(long, default_value = "default")]
        profile: String,
        /// Sign in with a device code instead of the browser (for SSH sessions)
        #[arg(long)]
        device: bool,
    },
    /// Remove a provider's stored credentials
    Logout {
        /// The provider, e.g. openai
        provider: String,
        /// The account profile (default: the one the provider uses)
        #[arg(long)]
        profile: Option<String>,
    },
    /// Review the workspace's project settings that widen what the agent may do, and trust them
    Trust {
        /// Trust without asking (for scripts)
        #[arg(long)]
        yes: bool,
        /// Remove trust for this workspace
        #[arg(long, conflicts_with = "yes")]
        revoke: bool,
    },
    /// Inspect the OS sandbox
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// Report model usage and cost from the local ledger, by model, provider, day or project
    Usage {
        /// What to group by: model, provider, day or project
        #[arg(long, default_value = "model")]
        by: String,
        /// Only requests on or after this UTC date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Only requests on or before this UTC date (YYYY-MM-DD)
        #[arg(long)]
        until: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Store an API key for a provider, read from standard input (typed without echo, or piped)
    Add {
        /// The provider, e.g. openai
        provider: String,
        /// The account profile to store it under
        #[arg(long, default_value = "default")]
        profile: String,
    },
    /// Make a stored account profile the one a provider uses
    Use {
        /// The provider, e.g. openai
        provider: String,
        /// The account profile
        profile: String,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Show which sandbox this system gets, how git metadata is protected, and how to improve it
    Doctor,
}

/// What `--resume` without an id says when a subcommand follows it.
const RESUME_NEEDS_AN_ID: &str = "error: --resume needs an id when a command follows it; run `harness --resume` on its own to list this project's sessions";

/// Whether a bare `--resume` in `args` is followed directly by a subcommand, which clap would
/// take for its id.
fn resume_before_a_subcommand(args: &[std::ffi::OsString]) -> bool {
    let command = Cli::command();
    let is_subcommand = |arg: &std::ffi::OsString| {
        arg == "help" || command.get_subcommands().any(|c| arg == c.get_name())
    };
    let args: Vec<_> = args.iter().skip(1).take_while(|a| *a != "--").collect();
    args.windows(2)
        .any(|pair| pair[0] == "--resume" && is_subcommand(pair[1]))
}

/// What `ask --resume <prompt>` says: clap took the prompt for the id, and found no prompt.
const RESUME_TOOK_THE_PROMPT: &str = "error: --resume needs an id, followed by the prompt: `harness ask --resume <ID> \"<prompt>\"`; run `harness --resume` on its own to list this project's sessions";

/// Whether `--resume` follows `ask` in `args` with a value, which clap takes for the id even when
/// it was meant as the prompt.
fn resume_after_ask(args: &[std::ffi::OsString]) -> bool {
    let args: Vec<_> = args.iter().skip(1).take_while(|a| *a != "--").collect();
    let Some(ask) = args.iter().position(|a| *a == "ask") else {
        return false;
    };
    let after = &args[ask + 1..];
    after
        .iter()
        .any(|a| a.to_string_lossy().starts_with("--resume="))
        || after
            .windows(2)
            .any(|pair| pair[0] == "--resume" && !pair[1].to_string_lossy().starts_with('-'))
}

/// How `command` is typed, for messages.
fn command_line(command: &Command) -> &'static str {
    match command {
        Command::Ask { .. } => "harness ask",
        Command::Models => "harness models",
        Command::Auth {
            command: AuthCommand::Add { .. },
        } => "harness auth add",
        Command::Auth {
            command: AuthCommand::Use { .. },
        } => "harness auth use",
        Command::Login { .. } => "harness login",
        Command::Logout { .. } => "harness logout",
        Command::Trust { .. } => "harness trust",
        Command::Sandbox { .. } => "harness sandbox doctor",
        Command::Usage { .. } => "harness usage",
    }
}

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if resume_before_a_subcommand(&args) {
        eprintln!("{RESUME_NEEDS_AN_ID}");
        return ExitCode::from(2);
    }
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e)
            if e.kind() == clap::error::ErrorKind::MissingRequiredArgument
                && resume_after_ask(&args) =>
        {
            eprintln!("{RESUME_TOOK_THE_PROMPT}");
            return ExitCode::from(2);
        }
        Err(e) => e.exit(),
    };
    // Only `ask` and the interactive session continue a session, and only `ask` has a run to
    // log: with another subcommand `-c`, `--resume` and `--debug` would do nothing.
    if let Some(command) = cli
        .command
        .as_ref()
        .filter(|c| !matches!(c, Command::Ask { .. }))
    {
        let flag = match (&cli.resume, cli.continue_session) {
            (Some(Some(_)), _) => Some("--resume"),
            (None, true) => Some("-c/--continue"),
            _ => None,
        };
        if let Some(flag) = flag {
            eprintln!(
                "error: {flag} continues a session, which only `harness ask` and `harness` alone do; run `{}` without it",
                command_line(command)
            );
            return ExitCode::from(2);
        }
        if cli.debug {
            eprintln!(
                "error: --debug logs a run of `harness ask`; run `{}` without it",
                command_line(command)
            );
            return ExitCode::from(2);
        }
    }
    // The interactive session (no subcommand) has no run to log either: unlike `ask`, it ignored
    // the flag silently rather than refusing it.
    if cli.command.is_none() && cli.debug {
        eprintln!("error: --debug logs a run of `harness ask`; run `harness` without it");
        return ExitCode::from(2);
    }
    let session = match (&cli.resume, cli.continue_session) {
        // Listing is what `--resume` alone does; with a subcommand, the id was forgotten.
        (Some(None), _) if cli.command.is_some() => {
            eprintln!("{RESUME_NEEDS_AN_ID}");
            return ExitCode::from(2);
        }
        // On a terminal, the session starts with the session picker open.
        (Some(None), _)
            if interactive::needs_terminal(
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
            )
            .is_some() =>
        {
            return ExitCode::from(sessions::print_list());
        }
        (Some(None), _) => sessions::Choice::New,
        (Some(Some(id)), _) => sessions::Choice::Resume(id.clone()),
        (None, true) => sessions::Choice::Continue,
        (None, false) => sessions::Choice::New,
    };
    // Before the runtime starts its threads: see `interactive::prepare`.
    if cli.command.is_none()
        && let Some(code) = interactive::prepare()
    {
        return ExitCode::from(code);
    }
    let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    let code = runtime.block_on(async move {
        match cli.command {
            Some(Command::Ask { json, prompt }) => {
                ask::run(
                    cli.model,
                    cli.mode,
                    session,
                    prompt.join(" "),
                    json,
                    cli.debug,
                )
                .await
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Auth {
                command: AuthCommand::Add { provider, profile },
            }) => auth::add(&provider, &profile),
            Some(Command::Auth {
                command: AuthCommand::Use { provider, profile },
            }) => auth::use_profile(&provider, &profile),
            Some(Command::Login {
                provider,
                profile,
                device,
            }) => login::run(&provider, &profile, device).await,
            Some(Command::Logout { provider, profile }) => {
                auth::logout(&provider, profile.as_deref()).await
            }
            Some(Command::Trust { yes, revoke }) => trust::run(yes, revoke),
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
            }) => doctor::run(),
            Some(Command::Usage { by, since, until }) => usage::report(&by, since, until),
            None => {
                let pick_session = matches!(cli.resume, Some(None));
                interactive::run(cli.model, cli.mode, session, pick_session).await
            }
        }
    });
    // A blocking task given up on (a file read for an approval's diff that never returned) must
    // not hold up the exit: dropping the runtime would wait for it.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    ExitCode::from(code)
}
