mod common;
use common::Isolate;

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
            .isolate()
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
            .isolate()
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
            .isolate()
            .assert()
            .code(2)
            .stderr(contains(format!(
                "{flag} continues a session, which only `harness ask` does; run `{command}` without it"
            )));
    }
}

// Spec: "Help output" lists the credential commands.
#[test]
fn help_lists_the_credential_commands() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("auth").and(contains("logout")));
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["auth", "--help"])
        .assert()
        .success()
        .stdout(contains("add").and(contains("use")));
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["auth", "add", "--help"])
        .assert()
        .success()
        .stdout(contains("--profile").and(contains("standard input")));
}

// Should the refusal break, `logout` would reach the keychain: only the debug-only test hook keeps
// it off the real one.
#[cfg(debug_assertions)]
#[test]
fn session_flags_with_the_credential_commands_are_refused() {
    let home = tempfile::tempdir().unwrap();
    for (args, command) in [
        (&["-c", "auth", "add", "openai"][..], "harness auth add"),
        (&["auth", "use", "openai", "work", "-c"], "harness auth use"),
        (&["logout", "openai", "--continue"], "harness logout"),
    ] {
        Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .env("HARNESS_HOME", home.path())
            .isolate()
            .write_stdin("sk-never-stored")
            .assert()
            .code(2)
            .stderr(contains(format!(
                "-c/--continue continues a session, which only `harness ask` does; run `{command}` without it"
            )));
    }
    assert!(!home.path().join("data/credentials.json").exists());
}

#[test]
fn help_lists_the_debug_flag() {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("--debug").and(contains("secrets redacted")));
}

// Review B, M12: a suite that lists models would send the developer's own provider keys to the
// real providers, so every run the suites start leaves them out.
#[test]
fn isolated_runs_carry_no_provider_key() {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_harness"));
    for var in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"] {
        cmd.env(var, "sk-developers-own");
    }
    cmd.isolate();
    let envs: Vec<_> = cmd.get_envs().collect();
    for var in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"] {
        assert!(
            envs.contains(&(std::ffi::OsStr::new(var), None)),
            "{var}: {envs:?}"
        );
    }
    assert!(envs.contains(&(
        std::ffi::OsStr::new("HARNESS_CREDENTIAL_STORE"),
        Some(std::ffi::OsStr::new("file"))
    )));
}
