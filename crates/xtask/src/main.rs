//! `cargo xtask eval <replay|record|oracles>`.

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use xtask::{oracle, record, replay, task};

#[derive(Parser)]
#[command(name = "xtask", about = "Developer tasks for this repository")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The edit-format eval suite in `eval/`.
    Eval {
        #[command(subcommand)]
        mode: Mode,
    },
}

#[derive(Subcommand)]
enum Mode {
    /// Apply the recorded outputs to every task, in every format, without a model.
    Replay,
    /// Write the recordings again from each task's `before` and `after`.
    Record,
    /// Check that each task's test fails before and passes after (needs the toolchains).
    Oracles,
}

fn eval_dir() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/tasks")
}

fn main() -> ExitCode {
    let Command::Eval { mode } = Cli::parse().command;
    let tasks = match task::load_all(&eval_dir()) {
        Ok(tasks) => tasks,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    match mode {
        Mode::Replay => {
            let failures = replay::check_all(&tasks);
            for failure in &failures {
                eprintln!("FAIL {failure}");
            }
            println!(
                "{} tasks x 4 formats: {} applied, {} failed",
                tasks.len(),
                tasks.len() * 4 - failures.len(),
                failures.len()
            );
            ExitCode::from(u8::from(!failures.is_empty()))
        }
        Mode::Record => match record::write_all(&tasks) {
            Ok(()) => {
                println!("recorded {} tasks x 4 formats", tasks.len());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Mode::Oracles => {
            let report = oracle::check_all(&tasks);
            for line in &report.skipped {
                println!("skipped {line}");
            }
            for line in &report.wrong {
                eprintln!("WRONG {line}");
            }
            println!(
                "{} oracles checked, {} skipped",
                report.checked,
                report.skipped.len()
            );
            ExitCode::from(u8::from(!report.wrong.is_empty()))
        }
    }
}
