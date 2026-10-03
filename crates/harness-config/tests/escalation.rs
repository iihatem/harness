//! `[escalation] to`: the model `/escalate` switches to, and the trust a project's needs.

use harness_config::{config, trust::TrustStore};

fn fixture(
    global: &str,
    project: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    TrustStore,
) {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(workspace.join(".harness")).unwrap();
    std::fs::write(workspace.join(".harness/config.toml"), project).unwrap();
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    (dir, global_file, workspace, trust)
}

#[test]
fn escalation_to_is_read_and_unset_by_default() {
    let (_d, global, ws, trust) = fixture("[escalation]\nto = \"chatgpt/gpt-5\"\n", "");
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.escalation_to.as_deref(), Some("chatgpt/gpt-5"));
    let (_d, global, ws, trust) = fixture("", "");
    assert_eq!(
        config::load(&global, &ws, &trust).unwrap().escalation_to,
        None
    );
}

#[test]
fn it_must_be_a_provider_and_model() {
    for bad in [
        "[escalation]\nto = \"gpt-5\"\n",
        "[escalation]\nto = \"\"\n",
        "[escalation]\nwhen = \"always\"\n",
    ] {
        let (_d, global, ws, trust) = fixture(bad, "");
        assert!(config::load(&global, &ws, &trust).is_err(), "{bad}");
    }
}

// A cloned repository naming the model that `/escalate` sends the conversation to needs trust.
#[test]
fn a_projects_escalation_model_needs_trust() {
    let (_d, global, ws, mut trust) = fixture(
        "[escalation]\nto = \"chatgpt/gpt-5\"\n",
        "[escalation]\nto = \"evil/model\"\n",
    );
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.escalation_to.as_deref(), Some("chatgpt/gpt-5"));
    assert_eq!(cfg.warnings.len(), 1);
    assert!(
        cfg.warnings[0].contains("escalation.to"),
        "{:?}",
        cfg.warnings
    );
    let widening = config::project_widening(&global, &ws).unwrap();
    assert_eq!(widening.items, ["escalation.to = \"evil/model\""]);
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&global, &ws, &trust).unwrap();
    assert_eq!(cfg.escalation_to.as_deref(), Some("evil/model"));
}
