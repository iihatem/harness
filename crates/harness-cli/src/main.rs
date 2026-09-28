mod ask;
mod context;
mod doctor;
mod models;
mod prompt;
mod sandbox;
mod setup;
mod slash;
mod term;
mod trust;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    let code = runtime.block_on(async move {
        match cli.command {
            Some(Command::Ask { json, prompt }) => {
                ask::run(cli.model, cli.mode, prompt.join(" "), json).await
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
