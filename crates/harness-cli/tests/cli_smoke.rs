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
