//! `[lsp]`: language servers can be turned off, replaced and given a wait; a project's commands
//! need trust.

use harness_config::{
    config::{self, LspConfig, LspServer},
    trust::TrustStore,
};

struct Fixture {
    _dir: tempfile::TempDir,
    global: std::path::PathBuf,
    workspace: std::path::PathBuf,
    trust: TrustStore,
}

fn fixture(global: &str, project: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(workspace.join(".harness")).unwrap();
    std::fs::write(workspace.join(".harness/config.toml"), project).unwrap();
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    Fixture {
        global: global_file,
        workspace,
        trust,
        _dir: dir,
    }
}

impl Fixture {
    fn load(&self) -> Result<config::Config, config::ConfigError> {
        config::load(&self.global, &self.workspace, &self.trust)
    }

    fn trust_now(&mut self) {
        let widening = config::project_widening(&self.global, &self.workspace).unwrap();
        self.trust
            .trust(&self.workspace, &widening.fingerprint)
            .unwrap();
    }
}

#[test]
fn the_defaults_are_on_with_a_two_second_wait() {
    let cfg = fixture("", "").load().unwrap();
    assert_eq!(
        cfg.lsp,
        LspConfig {
            enabled: true,
            wait_ms: 2_000,
            servers: Default::default()
        }
    );
}

// Spec "Override".
#[test]
fn a_python_command_is_overridden_in_lsp_servers() {
    let cfg = fixture("[lsp.servers.python]\ncommand = \"/opt/bin/pylsp\"\n", "")
        .load()
        .unwrap();
    assert_eq!(
        cfg.lsp.servers["python"],
        LspServer {
            command: Some("/opt/bin/pylsp".into()),
            enabled: true
        }
    );
}

// Spec "Disabled": all servers, or one language.
#[test]
fn servers_can_be_turned_off_altogether_or_per_language() {
    let cfg = fixture("[lsp]\nenabled = false\nwait_ms = 500\n", "")
        .load()
        .unwrap();
    assert!(!cfg.lsp.enabled);
    assert_eq!(cfg.lsp.wait_ms, 500);
    let cfg = fixture("[lsp.servers.go]\nenabled = false\n", "")
        .load()
        .unwrap();
    assert!(cfg.lsp.enabled && !cfg.lsp.servers["go"].enabled);
}

// Spec "Roles and language servers in an untrusted project": the custom command is ignored with
// a warning, and the default lookup applies.
#[test]
fn a_projects_server_commands_need_trust() {
    let mut f = fixture("", "[lsp.servers.python]\ncommand = \"./evil-server\"\n");
    let cfg = f.load().unwrap();
    assert!(
        cfg.lsp
            .servers
            .get("python")
            .is_none_or(|s| s.command.is_none())
    );
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(
        cfg.warnings[0].contains("lsp.servers.python"),
        "{:?}",
        cfg.warnings
    );
    let widening = config::project_widening(&f.global, &f.workspace).unwrap();
    assert_eq!(
        widening.items,
        ["lsp.servers.python.command = \"./evil-server\""]
    );
    f.trust_now();
    let cfg = f.load().unwrap();
    assert_eq!(
        cfg.lsp.servers["python"].command.as_deref(),
        Some("./evil-server")
    );
    assert!(cfg.warnings.is_empty());
}

// Turning servers off narrows: it applies without trust.
#[test]
fn a_project_may_turn_servers_off_or_shorten_the_wait_without_trust() {
    let f = fixture(
        "",
        "[lsp]\nenabled = false\nwait_ms = 300\n[lsp.servers.rust]\nenabled = false\n",
    );
    let cfg = f.load().unwrap();
    assert!(!cfg.lsp.enabled && cfg.lsp.wait_ms == 300 && !cfg.lsp.servers["rust"].enabled);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    assert!(
        config::project_widening(&f.global, &f.workspace)
            .unwrap()
            .items
            .is_empty()
    );
}

// Turning a server on again is a widening: ignored with a warning until the project is trusted,
// and then it applies.
#[test]
fn a_project_turns_back_on_what_the_global_config_turned_off_only_once_trusted() {
    let mut f = fixture(
        "[lsp]\nenabled = false\n[lsp.servers.go]\nenabled = false\n",
        "[lsp]\nenabled = true\n[lsp.servers.go]\nenabled = true\n",
    );
    let cfg = f.load().unwrap();
    assert!(!cfg.lsp.enabled && !cfg.lsp.servers["go"].enabled);
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(
        cfg.warnings[0].contains("lsp.enabled")
            && cfg.warnings[0].contains("lsp.servers.go.enabled"),
        "{:?}",
        cfg.warnings
    );
    f.trust_now();
    let cfg = f.load().unwrap();
    assert!(cfg.lsp.enabled && cfg.lsp.servers["go"].enabled);
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn invalid_lsp_settings_are_errors_naming_the_file() {
    for (text, problem) in [
        ("[lsp]\nwait_ms = 5\n", "wait_ms"),
        ("[lsp]\nwait_ms = 600000\n", "wait_ms"),
        ("[lsp.servers.cobol]\ncommand = \"x\"\n", "cobol"),
        ("[lsp.servers.go]\ncommand = \"  \"\n", "lsp.servers.go"),
        ("[lsp.servers.go]\nargs = 1\n", "args"),
    ] {
        let error = fixture(text, "").load().unwrap_err().to_string();
        assert!(
            error.contains("config.toml") && error.contains(problem),
            "{text}: {error}"
        );
        assert!(fixture("", text).load().is_err(), "{text}");
    }
}
