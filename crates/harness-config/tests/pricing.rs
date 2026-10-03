//! `[pricing."<glob>"]`: the user's own prices, per million tokens, over the price tables'.

use harness_config::{config, trust::TrustStore};

fn load(global: &str, project: Option<&str>) -> Result<config::Config, config::ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    if let Some(project) = project {
        std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    }
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    config::load(&global_file, &ws, &trust)
}

#[test]
fn prices_are_read_by_glob() {
    let cfg = load(
        "[pricing.\"openai/gpt-5*\"]\ninput = 2.00\noutput = 12\ncache_read = 0.2\n",
        None,
    )
    .unwrap();
    let price = &cfg.pricing["openai/gpt-5*"];
    assert_eq!(price.input, Some(2.0));
    assert_eq!(price.output, Some(12.0));
    assert_eq!(price.cache_read, Some(0.2));
    assert_eq!(price.cache_write, None);
}

#[test]
fn a_price_that_cannot_be_right_is_an_error() {
    for bad in [
        "[pricing.\"openai/[\"]\ninput = 1\n",
        "[pricing.\"x/*\"]\ninput = -1\n",
        "[pricing.\"x/*\"]\ninput = \"cheap\"\n",
        "[pricing.\"x/*\"]\ncolour = 1\n",
    ] {
        let err = load(bad, None).unwrap_err().to_string();
        assert!(
            err.contains("pricing") || err.contains("invalid config"),
            "{bad}: {err}"
        );
    }
}

// Prices are the user's: a cloned repository cannot make a model look free.
#[test]
fn a_project_config_cannot_set_prices() {
    let cfg = load(
        "[pricing.\"x/*\"]\ninput = 1\noutput = 2\n",
        Some("[pricing.\"x/*\"]\ninput = 0\noutput = 0\n"),
    )
    .unwrap();
    assert_eq!(cfg.pricing["x/*"].input, Some(1.0));
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("[pricing]") && w.contains("global config")),
        "{:?}",
        cfg.warnings
    );
}

// `[usage] baseline` names the model "avoided" is measured against; global config only.
#[test]
fn the_baseline_is_read_from_the_global_config_only() {
    let cfg = load(
        "[usage]\nbaseline = \"openai/gpt-5\"\n",
        Some("[usage]\nbaseline = \"openai/gpt-5-nano\"\n"),
    )
    .unwrap();
    assert_eq!(cfg.usage.baseline.as_deref(), Some("openai/gpt-5"));
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("[usage]") && w.contains("global config")),
        "{:?}",
        cfg.warnings
    );
    assert_eq!(load("", None).unwrap().usage.baseline, None);
}
