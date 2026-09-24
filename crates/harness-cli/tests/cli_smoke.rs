use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn prints_version() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("harness 0.1.0"));
}
