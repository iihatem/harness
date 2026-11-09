//! `[agents]`: how many agents run at once, and how deep and how many they spawn. Global only: a
//! project cannot make harness spend more.

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
fn the_defaults_are_one_per_local_server_three_per_provider_four_in_all() {
    let cfg = load("", "").unwrap();
    assert_eq!(cfg.agents.per_local, 1);
    assert_eq!(cfg.agents.per_hosted, 3);
    assert_eq!(cfg.agents.max_total, 4);
    assert_eq!(cfg.agents.max_depth, 1);
    assert_eq!(cfg.agents.max_spawns_per_turn, 10);
}

#[test]
fn each_limit_can_be_set() {
    let cfg = load(
        "[agents]\nper_local = 2\nper_hosted = 5\nmax_total = 8\nmax_depth = 2\nmax_spawns_per_turn = 20\n",
        "",
    )
    .unwrap();
    assert_eq!(
        (
            cfg.agents.per_local,
            cfg.agents.per_hosted,
            cfg.agents.max_total
        ),
        (2, 5, 8)
    );
    assert_eq!(
        (cfg.agents.max_depth, cfg.agents.max_spawns_per_turn),
        (2, 20)
    );
}

#[test]
fn zero_and_unknown_keys_are_errors() {
    for bad in [
        "[agents]\nmax_total = 0\n",
        "[agents]\nper_local = 0\n",
        "[agents]\nparallel = 3\n",
    ] {
        assert!(load(bad, "").is_err(), "{bad}");
    }
}

#[test]
fn a_project_cannot_set_them() {
    let cfg = load("", "[agents]\nmax_total = 50\n").unwrap();
    assert_eq!(cfg.agents.max_total, 4);
    assert!(
        cfg.warnings.iter().any(|w| w.contains("[agents]")),
        "{:?}",
        cfg.warnings
    );
}
