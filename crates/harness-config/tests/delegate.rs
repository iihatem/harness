//! `[delegate]`: how `claude-code/*` and `codex/*` models run. Global only: a cloned repository
//! must not opt the user in to using their subscription.

use harness_config::{config, trust::TrustStore};

fn load(global: &str, project: &str) -> Result<config::Config, config::ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(workspace.join(".harness")).unwrap();
    std::fs::write(workspace.join(".harness/config.toml"), project).unwrap();
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    config::load(&global_file, &workspace, &trust)
}

#[test]
fn nothing_is_opted_into_by_default() {
    let cfg = load("", "").unwrap();
    assert_eq!(cfg.delegate.claude_subscription, None);
    assert!(!cfg.delegate.allow_untested);
}

#[test]
fn the_global_config_sets_both() {
    let cfg = load(
        "[delegate]\nallow_untested = true\n[delegate.claude]\nuse_subscription = true\n",
        "",
    )
    .unwrap();
    assert_eq!(cfg.delegate.claude_subscription, Some(true));
    assert!(cfg.delegate.allow_untested);
    let off = load("[delegate.claude]\nuse_subscription = false\n", "").unwrap();
    assert_eq!(off.delegate.claude_subscription, Some(false));
}

#[test]
fn unknown_keys_are_errors() {
    assert!(load("[delegate]\nbinary = \"/bin/x\"\n", "").is_err());
    assert!(load("[delegate.claude]\nbinary = \"/bin/x\"\n", "").is_err());
}

#[test]
fn a_project_cannot_opt_in() {
    let cfg = load("", "[delegate.claude]\nuse_subscription = true\n").unwrap();
    assert_eq!(cfg.delegate.claude_subscription, None);
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("ignoring [delegate]")),
        "{:?}",
        cfg.warnings
    );
}
