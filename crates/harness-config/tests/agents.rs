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
    // Claude Code and Codex are each limited to two at once, inside the four.
    assert_eq!(cfg.agents.per_delegated, 2);
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
fn the_limit_per_delegated_program_can_be_set() {
    let cfg = load("[agents]\nper_delegated = 1\n", "").unwrap();
    assert_eq!(cfg.agents.per_delegated, 1);
    assert_eq!(cfg.agents.per_hosted, 3);
}

#[test]
fn zero_and_unknown_keys_are_errors() {
    for bad in [
        "[agents]\nmax_total = 0\n",
        "[agents]\nper_local = 0\n",
        "[agents]\nper_delegated = 0\n",
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

#[test]
fn spawn_is_on_by_default_and_the_global_config_can_turn_it_off() {
    assert!(load("", "").unwrap().agents.spawn);
    assert!(!load("[agents]\nspawn = false\n", "").unwrap().agents.spawn);
}

#[test]
fn a_project_can_turn_spawn_off_but_never_on() {
    let off = load("", "[agents]\nspawn = false\n").unwrap();
    assert!(!off.agents.spawn);
    assert!(
        off.warnings.iter().all(|w| !w.contains("[agents]")),
        "{:?}",
        off.warnings
    );
    let still_off = load("[agents]\nspawn = false\n", "[agents]\nspawn = true\n").unwrap();
    assert!(!still_off.agents.spawn);
    assert!(
        still_off.warnings.iter().any(|w| w.contains("spawn")),
        "{:?}",
        still_off.warnings
    );
}

#[test]
fn a_depth_above_one_is_said_to_have_no_effect() {
    let cfg = load("[agents]\nmax_depth = 3\n", "").unwrap();
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("max_depth") && w.contains("no effect")),
        "{:?}",
        cfg.warnings
    );
    let one = load("[agents]\nmax_depth = 1\n", "").unwrap();
    assert!(one.warnings.iter().all(|w| !w.contains("max_depth")));
}

#[test]
fn a_zero_in_a_projects_agents_is_ignored_with_a_warning_and_does_not_stop_trust() {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, "").unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(workspace.join(".harness")).unwrap();
    std::fs::write(
        workspace.join(".harness/config.toml"),
        "[agents]\nmax_total = 0\n",
    )
    .unwrap();
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    // The session reads it, ignoring [agents] with a warning ...
    let cfg = config::load(&global_file, &workspace, &trust).unwrap();
    assert_eq!(cfg.agents.max_total, 4);
    // ... so `harness trust` must not fail on it either.
    assert!(config::project_widening(&global_file, &workspace).is_ok());
}
