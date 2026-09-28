mod ask;
mod context;
mod doctor;
mod models;
mod prompt;
mod sandbox;
mod sessions;
mod setup;
mod slash;
mod term;
mod trust;

use std::process::ExitCode;

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

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if resume_before_a_subcommand(&args) {
        eprintln!("{RESUME_NEEDS_AN_ID}");
        return ExitCode::from(2);
    }
    let cli = Cli::parse_from(args);
    let session = match (&cli.resume, cli.continue_session) {
        // Listing is what `--resume` alone does; with a subcommand, the id was forgotten.
        (Some(None), _) if cli.command.is_some() => {
            eprintln!("{RESUME_NEEDS_AN_ID}");
            return ExitCode::from(2);
        }
        (Some(None), _) => return ExitCode::from(sessions::print_list()),
        (Some(Some(id)), _) => sessions::Choice::Resume(id.clone()),
        (None, true) => sessions::Choice::Continue,
        (None, false) => sessions::Choice::New,
    };
    let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    let code = runtime.block_on(async move {
        match cli.command {
            Some(Command::Ask { json, prompt }) => {
                ask::run(cli.model, cli.mode, session, prompt.join(" "), json).await
            }
            Some(Command::Models) => models::run().await,
            Some(Command::Trust { yes, revoke }) => trust::run(yes, revoke),
            Some(Command::Sandbox {
                command: SandboxCommand::Doctor,
            }) => doctor::run(),
            None => {
                eprintln!("Interactive mode is not available yet; use `harness ask \"...\"`.");
                2
            }
        }
    });
    ExitCode::from(code)
}
