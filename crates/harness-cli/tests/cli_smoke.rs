use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;

#[test]
fn prints_version() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("harness 0.1.0"));
}

#[test]
fn help_lists_the_sandbox_doctor() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("sandbox").and(contains("Inspect the OS sandbox")));
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["sandbox", "--help"])
        .assert()
        .success()
        .stdout(contains("doctor").and(contains(
            "Show which sandbox this system gets, how git metadata is protected, and how to improve it",
        )));
}

#[test]
fn help_lists_the_session_flags() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("-c, --continue"))
        .stdout(contains("--resume [<ID>]"));
}

// Review D M4: `--resume` without an id lists sessions only on its own; with a subcommand it is
// a mistake, and the subcommand must not run (or be taken for the id).
#[test]
fn resume_without_an_id_before_a_subcommand_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["models", "--resume"][..],
        &["trust", "--resume"],
        &["--resume", "ask", "hi"],
        &["--resume", "models"],
    ] {
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .assert()
            .code(2)
            .stderr(contains("--resume needs an id"));
    }
}

// Final review, minor 2: `--resume` takes an id, so in `harness ask --resume "fix it"` the prompt
// was taken for one. The error says what `--resume` needs rather than that the prompt is missing.
#[test]
fn resume_taking_the_prompt_for_its_id_is_explained() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["ask", "--resume", "fix it"][..],
        &["ask", "--json", "--resume", "fix it"],
    ] {
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .assert()
            .code(2)
            .stderr(contains("--resume needs an id, followed by the prompt"));
    }
}

// Only `ask` continues a session: with another subcommand `-c` and `--resume <id>` were ignored
// silently.
#[test]
fn session_flags_with_another_subcommand_are_refused() {
    let home = tempfile::tempdir().unwrap();
    for (args, flag, command) in [
        (&["-c", "models"][..], "-c/--continue", "harness models"),
        (&["models", "--continue"], "-c/--continue", "harness models"),
        (&["trust", "-c", "--yes"], "-c/--continue", "harness trust"),
        (
            &["--resume", "20260927T123456Z-1a2b3c4d", "models"],
            "--resume",
            "harness models",
        ),
        (&["trust", "--resume", "abc"], "--resume", "harness trust"),
        (
            &["sandbox", "doctor", "-c"],
            "-c/--continue",
            "harness sandbox doctor",
        ),
    ] {
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .assert()
            .code(2)
            .stderr(contains(format!(
                "{flag} continues a session, which only `harness ask` does; run `{command}` without it"
            )));
    }
}
