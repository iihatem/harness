//! `[roles]`: which model each role runs on, the hand-off mode, and the trust a project's roles
//! need.

use harness_config::{config, trust::TrustStore};
use harness_core::role::HandoffMode;

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

// Spec "Defaults": no `[roles]` table, so no role is set and the hand-off follows the history.
#[test]
fn without_a_roles_table_no_role_is_set() {
    let cfg = fixture("", "").load().unwrap();
    assert_eq!(cfg.roles, Default::default());
    assert_eq!(cfg.roles.main, None);
    assert_eq!(cfg.roles.handoff, None);
}

// Spec "Partial configuration": only `plan` is set.
#[test]
fn a_partial_table_sets_only_the_roles_it_names() {
    let cfg = fixture("[roles]\nplan = \"chatgpt/gpt-5\"\n", "")
        .load()
        .unwrap();
    assert_eq!(cfg.roles.plan.as_deref(), Some("chatgpt/gpt-5"));
    assert_eq!(cfg.roles.main, None);
    assert_eq!(cfg.roles.build, None);
    assert_eq!(cfg.roles.background, None);
}

#[test]
fn every_role_and_the_handoff_mode_are_read() {
    let cfg = fixture(
        "[roles]\nmain = \"ollama/llama3\"\nplan = \"chatgpt/gpt-5\"\nbuild = \"ollama/qwen3-coder\"\nbackground = \"ollama/llama3\"\n[roles.handoff]\nmode = \"plan_only\"\n",
        "",
    )
    .load()
    .unwrap();
    assert_eq!(cfg.roles.main.as_deref(), Some("ollama/llama3"));
    assert_eq!(cfg.roles.build.as_deref(), Some("ollama/qwen3-coder"));
    assert_eq!(cfg.roles.background.as_deref(), Some("ollama/llama3"));
    assert_eq!(cfg.roles.handoff, Some(HandoffMode::PlanOnly));
    let history = fixture("[roles.handoff]\nmode = \"history\"\n", "")
        .load()
        .unwrap();
    assert_eq!(history.roles.handoff, Some(HandoffMode::History));
}

#[test]
fn a_role_must_be_a_provider_and_model_and_the_mode_one_of_two_words() {
    let bad = fixture("[roles]\nbuild = \"qwen3-coder\"\n", "").load();
    let message = bad.unwrap_err().to_string();
    assert!(message.contains("roles.build"), "{message}");
    assert!(message.contains("provider/model"), "{message}");
    assert!(fixture("[roles]\nplan = \"\"\n", "").load().is_err());
    assert!(
        fixture("[roles.handoff]\nmode = \"everything\"\n", "")
            .load()
            .is_err()
    );
    // An unknown role is a typo, not a role.
    assert!(fixture("[roles]\nreview = \"a/b\"\n", "").load().is_err());
}

// Spec "Untrusted project roles": the build turn runs on what the user-level config or `main`
// selects, and a notice says the project's roles were ignored.
#[test]
fn a_projects_roles_need_trust() {
    let mut f = fixture(
        "[roles]\nbuild = \"ollama/qwen3-coder\"\n",
        "[roles]\nbuild = \"openrouter/some/model\"\n[roles.handoff]\nmode = \"history\"\n",
    );
    let cfg = f.load().unwrap();
    assert_eq!(cfg.roles.build.as_deref(), Some("ollama/qwen3-coder"));
    assert_eq!(cfg.roles.handoff, None);
    assert!(!cfg.trusted);
    assert_eq!(cfg.warnings.len(), 1, "{:?}", cfg.warnings);
    assert!(
        cfg.warnings[0].contains("roles.build"),
        "{:?}",
        cfg.warnings
    );

    let widening = config::project_widening(&f.global, &f.workspace).unwrap();
    assert_eq!(
        widening.items,
        [
            "roles.build = \"openrouter/some/model\"",
            "roles.handoff.mode = \"history\"",
        ]
    );

    f.trust_now();
    let cfg = f.load().unwrap();
    assert_eq!(cfg.roles.build.as_deref(), Some("openrouter/some/model"));
    assert_eq!(cfg.roles.handoff, Some(HandoffMode::History));
    assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
}

#[test]
fn a_trusted_projects_roles_go_over_the_global_ones_role_by_role() {
    let mut f = fixture(
        "[roles]\nplan = \"chatgpt/gpt-5\"\nbuild = \"ollama/a\"\n",
        "[roles]\nbuild = \"ollama/b\"\n",
    );
    f.trust_now();
    let cfg = f.load().unwrap();
    assert_eq!(cfg.roles.plan.as_deref(), Some("chatgpt/gpt-5"));
    assert_eq!(cfg.roles.build.as_deref(), Some("ollama/b"));
}

#[test]
fn roles_changed_after_trust_are_untrusted_again() {
    let mut f = fixture("", "[roles]\nplan = \"a/b\"\n");
    f.trust_now();
    assert_eq!(f.load().unwrap().roles.plan.as_deref(), Some("a/b"));
    std::fs::write(
        f.workspace.join(".harness/config.toml"),
        "[roles]\nplan = \"a/evil\"\n",
    )
    .unwrap();
    let cfg = f.load().unwrap();
    assert_eq!(cfg.roles.plan, None);
    assert_eq!(cfg.warnings.len(), 1);
}
