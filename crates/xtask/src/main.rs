//! `cargo xtask eval <replay|record|oracles>`.

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use harness_core::edit_format::EditFormat;
use xtask::{live, oracle, record, replay, task};

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
    /// Run a model on the tasks (live mode): `cargo xtask eval run --model ollama/qwen3-coder:30b
    /// --format apply_patch -n 3`. It starts `harness ask` for each run, so the model and its
    /// provider must work for `harness` as configured; sign-ins and stored keys are not carried
    /// over, but keys in the environment and a local server are.
    Run {
        /// `<provider>/<model>`.
        #[arg(long)]
        model: String,
        /// One of str_replace, apply_patch, whole_file, hashline.
        #[arg(long, value_parser = |s: &str| s.parse::<EditFormat>())]
        format: EditFormat,
        /// How many times each task runs.
        #[arg(short = 'n', default_value_t = 3)]
        runs: u32,
        /// Only these tasks (by id); default all.
        #[arg(long = "task")]
        tasks: Vec<String>,
        /// The `harness` binary; default is built from this checkout.
        #[arg(long)]
        harness: Option<PathBuf>,
        /// The configuration the run's is made from; default is your own.
        #[arg(long)]
        config: Option<PathBuf>,
        /// How long one run may take, in seconds.
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// Save the report in `eval/results/` for checking in.
        #[arg(long)]
        save: bool,
    },
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
        Mode::Run {
            model,
            format,
            runs,
            tasks: wanted,
            harness,
            config,
            timeout,
            save,
        } => {
            let tasks: Vec<task::Task> = tasks
                .into_iter()
                .filter(|t| wanted.is_empty() || wanted.contains(&t.id))
                .collect();
            if tasks.is_empty() {
                eprintln!("error: no task matches");
                return ExitCode::from(2);
            }
            let harness = match harness.map(Ok).unwrap_or_else(build_harness) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let options = live::Options {
                model,
                format,
                runs,
                harness,
                user_config: config.or_else(default_config),
                timeout: std::time::Duration::from_secs(timeout),
                env: Vec::new(),
            };
            let report = match live::run(&tasks, &options) {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            for run in &report.runs {
                println!(
                    "{} #{}: {}{}",
                    run.task,
                    run.run,
                    if run.passed { "pass" } else { "FAIL" },
                    run.error
                        .as_deref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                );
            }
            print!("{}", report.table());
            if save {
                let results = eval_dir().join("../results");
                match live::save(&report, &results) {
                    Ok(path) => println!("saved {}", path.display()),
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                }
            }
            ExitCode::SUCCESS
        }
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

/// Builds `harness` in this checkout, and where it is.
fn build_harness() -> Result<PathBuf, String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new("cargo")
        .args(["build", "-p", "harness-cli"])
        .current_dir(&root)
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err("building harness failed".into());
    }
    Ok(root.join("target/debug/harness"))
}

/// Your own configuration file, where `harness` looks for it.
fn default_config() -> Option<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let path = match (var("HARNESS_HOME"), var("XDG_CONFIG_HOME"), var("HOME")) {
        (Some(home), _, _) => home.join("config/config.toml"),
        (None, Some(xdg), _) => xdg.join("harness/config.toml"),
        (None, None, Some(home)) => home.join(".config/harness/config.toml"),
        _ => return None,
    };
    path.is_file().then_some(path)
}
