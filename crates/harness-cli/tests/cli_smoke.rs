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
