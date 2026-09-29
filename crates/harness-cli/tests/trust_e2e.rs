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
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
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
