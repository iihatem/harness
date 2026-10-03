//! The oracles: each task's test fails on its `before` tree and passes on its `after` tree. It
//! needs the language's toolchain, so a task whose toolchain is missing is skipped.

use std::process::Command;

use crate::task::{Task, Tree, write_tree};

/// The program a language's test command starts with.
fn toolchain(language: &str) -> &'static str {
    match language {
        "rust" => "cargo",
        "python" => "python3",
        "typescript" => "deno",
        _ => "go",
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Runs the task's test on `tree`; whether it passed.
pub fn passes(task: &Task, tree: &Tree) -> Result<bool, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    write_tree(dir.path(), tree)?;
    passes_in(
        dir.path(),
        &task.test,
        &std::env::temp_dir().join("harness-eval-target"),
    )
    .map_err(|e| format!("{}: {e}", task.id))
}

/// Runs `test` in `dir`, building into `target` if it is a Rust project; whether it exited 0.
pub fn passes_in(
    dir: &std::path::Path,
    test: &str,
    target: &std::path::Path,
) -> Result<bool, String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(test)
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cannot run `{test}`: {e}"))?;
    Ok(status.success())
}

/// What checking the oracles of `tasks` found: a line for each task that could not be checked
/// (`skipped`) or whose test is wrong (`wrong`).
#[derive(Debug, Default)]
pub struct Report {
    pub checked: usize,
    pub skipped: Vec<String>,
    pub wrong: Vec<String>,
}

pub fn check_all(tasks: &[Task]) -> Report {
    let mut report = Report::default();
    for task in tasks {
        let program = toolchain(&task.language);
        if !on_path(program) {
            report
                .skipped
                .push(format!("{}: `{program}` is not installed", task.id));
            continue;
        }
        report.checked += 1;
        match (passes(task, &task.before), passes(task, &task.after)) {
            (Ok(false), Ok(true)) => {}
            (Ok(before), Ok(after)) => report.wrong.push(format!(
                "{}: the test {} before and {} after (it should fail, then pass)",
                task.id,
                if before { "passes" } else { "fails" },
                if after { "passes" } else { "fails" },
            )),
            (Err(e), _) | (_, Err(e)) => report.wrong.push(e),
        }
    }
    report
}
