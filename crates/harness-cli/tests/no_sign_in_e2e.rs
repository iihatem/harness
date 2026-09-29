//! A build without ChatGPT sign-in (`--no-default-features`): what it says when asked for it,
//! and that no hint points to a `harness login chatgpt` it does not have. CI runs these with
//! `cargo test -p harness-cli --no-default-features --test no_sign_in_e2e`. `harness logout`
//! stays off the real keychain through a test hook that release builds ignore, so the suite is
//! built in debug builds only.
#![cfg(all(not(feature = "chatgpt-login"), debug_assertions))]

mod common;
use common::Isolate;

use assert_cmd::Command;
use predicates::prelude::*;
use predicates::str::contains;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_harness");

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new() -> Env {
        let env = Env {
            home: tempfile::tempdir().unwrap(),
            ws: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir(env.ws.path().join(".git")).unwrap();
        env
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .isolate();
        cmd
    }
}

const UNAVAILABLE: &str = "made without ChatGPT sign-in";

// Review C, M5.
#[test]
fn a_build_without_sign_in_says_so_and_never_points_to_it() {
    let env = Env::new();
    for args in [
        &["login", "chatgpt"][..],
        &["login", "chatgpt", "--device"],
        &["--model", "chatgpt/gpt-5.5", "ask", "hi"],
        &["auth", "add", "chatgpt"],
        // Final review, M-4: refused like `auth add chatgpt`, rather than recording a profile.
        &["auth", "use", "chatgpt", "work"],
    ] {
        env.cmd()
            .args(args)
            .write_stdin("k\n")
            .assert()
            .code(2)
            .stderr(contains(UNAVAILABLE))
            .stderr(contains("harness login").not());
    }
    assert!(!env.home.path().join("data/accounts.toml").exists());
    env.cmd()
        .args(["logout", "chatgpt"])
        .assert()
        .success()
        .stdout(contains("No credentials are stored for chatgpt"));
}
