mod common;
use common::Isolate;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_harness");

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(project_config: Option<&str>) -> Env {
        let env = Env {
            home: tempfile::tempdir().unwrap(),
            ws: tempfile::tempdir().unwrap(),
        };
        if let Some(text) = project_config {
            std::fs::create_dir_all(env.ws.path().join(".harness")).unwrap();
            std::fs::write(env.ws.path().join(".harness/config.toml"), text).unwrap();
        }
        env
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

/// The text of the trust file under the isolated home, empty when there is none.
fn trust_file(env: &Env) -> String {
    fn find(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == "trust.toml") {
                return Some(path);
            }
            if path.is_dir()
                && let Some(found) = find(&path)
            {
                return Some(found);
            }
        }
        None
    }
    find(env.home.path())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}

// Ruling P3: `harness trust` also enables language servers, and revoking takes that back.
#[test]
fn trusting_a_workspace_enables_its_language_servers_and_revoking_disables_them() {
    let env = Env::new(None);
    env.cmd()
        .args(["trust", "--yes"])
        .assert()
        .success()
        .stdout(contains("Language servers may start"));
    assert!(
        trust_file(&env).contains("[servers]"),
        "{}",
        trust_file(&env)
    );
    assert!(trust_file(&env).contains("= true"), "{}", trust_file(&env));
    env.cmd().args(["trust", "--revoke"]).assert().success();
    assert!(
        !trust_file(&env).contains("[servers]"),
        "{}",
        trust_file(&env)
    );
}

const PROJECT: &str = "[permissions]\nallow = [\"bash:make*\"]\n";

// Ruling P3-R1: a workspace without widening settings can still be trusted (for its command
// files), with the same confirmation.
#[test]
fn a_workspace_without_widening_settings_can_be_trusted() {
    for project in [None, Some("[permissions]\ndeny = [\"bash:curl*\"]\n")] {
        let env = Env::new(project);
        env.cmd()
            .arg("trust")
            .assert()
            .code(2)
            .stdout(contains("No project settings"))
            .stderr(contains("--yes"));
        env.cmd()
            .args(["trust", "--yes"])
            .assert()
            .success()
            .stdout(contains("Trusted"));
        env.cmd()
            .args(["trust", "--revoke"])
            .assert()
            .success()
            .stdout(contains("Revoked"));
    }
}

#[test]
fn non_interactive_trust_needs_yes() {
    let env = Env::new(Some(PROJECT));
    env.cmd()
        .arg("trust")
        .assert()
        .code(2)
        .stderr(contains("--yes"));
}

#[test]
fn trusting_applies_the_settings_and_revoking_removes_them() {
    let env = Env::new(Some(PROJECT));
    env.cmd()
        .arg("models")
        .assert()
        .success()
        .stderr(contains("harness trust"));

    env.cmd()
        .args(["trust", "--yes"])
        .assert()
        .success()
        .stdout(contains("permissions.allow: \"bash:make*\""))
        .stdout(contains("Trusted"));
    let after = env.cmd().arg("models").output().unwrap();
    assert!(!String::from_utf8_lossy(&after.stderr).contains("harness trust"));

    env.cmd()
        .args(["trust", "--revoke"])
        .assert()
        .success()
        .stdout(contains("Revoked"));
    env.cmd()
        .arg("models")
        .assert()
        .success()
        .stderr(contains("harness trust"));
}

#[test]
fn changing_trusted_settings_needs_trust_again() {
    let env = Env::new(Some(PROJECT));
    env.cmd().args(["trust", "--yes"]).assert().success();
    std::fs::write(
        env.ws.path().join(".harness/config.toml"),
        "[permissions]\nallow = [\"bash:make*\", \"bash:rm*\"]\n",
    )
    .unwrap();
    env.cmd()
        .arg("models")
        .assert()
        .success()
        .stderr(contains("harness trust"));
}

// Re-review of fix wave 4, nit: a repository's directory names reach `harness trust`'s output,
// so the paths it prints are made terminal-safe like any other text from the repository.
#[test]
fn trust_prints_directory_names_terminal_safe() {
    let env = Env::new(None);
    std::fs::create_dir(env.ws.path().join(".git")).unwrap();
    let sub = env.ws.path().join("sub\u{1b}[2J");
    std::fs::create_dir(&sub).unwrap();
    let mut trust = env.cmd();
    let output = trust
        .current_dir(&sub)
        .args(["trust", "--yes"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains("sub\\u{1b}[2J"), "{stdout:?}");
    let revoke = env
        .cmd()
        .current_dir(&sub)
        .args(["trust", "--revoke"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&revoke.stdout).contains('\u{1b}'));
}
