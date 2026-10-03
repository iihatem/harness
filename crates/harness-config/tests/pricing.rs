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

// `[budgets]`: money limits on billed cost. A project may only tighten them.
#[test]
fn budgets_are_read_and_checked() {
    let cfg = load(
        "[budgets]\nsession_usd = 1.5\ndaily_usd = 5\nmonthly_usd = 50\n",
        None,
    )
    .unwrap();
    assert_eq!(cfg.budgets.session_usd, Some(1.5));
    assert_eq!(cfg.budgets.daily_usd, Some(5.0));
    assert_eq!(cfg.budgets.monthly_usd, Some(50.0));
    assert_eq!(load("", None).unwrap().budgets.session_usd, None);
    for bad in [
        "[budgets]\nsession_usd = 0\n",
        "[budgets]\ndaily_usd = -1\n",
        "[budgets]\nmonthly_usd = \"lots\"\n",
        "[budgets]\nweekly_usd = 1\n",
    ] {
        assert!(load(bad, None).is_err(), "{bad}");
    }
}

#[test]
fn a_project_can_lower_a_budget_and_never_raise_it() {
    let cfg = load(
        "[budgets]\nsession_usd = 5\ndaily_usd = 10\n",
        Some("[budgets]\nsession_usd = 2\ndaily_usd = 100\nmonthly_usd = 20\n"),
    )
    .unwrap();
    assert_eq!(cfg.budgets.session_usd, Some(2.0), "lowered");
    assert_eq!(cfg.budgets.daily_usd, Some(10.0), "not raised");
    assert_eq!(
        cfg.budgets.monthly_usd,
        Some(20.0),
        "a limit where there was none"
    );
    assert!(
        cfg.warnings
            .iter()
            .any(|w| w.contains("budgets.daily_usd") && w.contains("raise")),
        "{:?}",
        cfg.warnings
    );
}

// `usage.auto_resume` is `ask` (the default) or `never`; there is no `always`.
#[test]
fn auto_resume_asks_by_default_and_can_be_turned_off() {
    use harness_config::config::AutoResume;
    assert_eq!(load("", None).unwrap().usage.auto_resume, AutoResume::Ask);
    assert_eq!(
        load("[usage]\nauto_resume = \"never\"\n", None)
            .unwrap()
            .usage
            .auto_resume,
        AutoResume::Never
    );
    assert_eq!(
        load("[usage]\nauto_resume = \"ask\"\n", None)
            .unwrap()
            .usage
            .auto_resume,
        AutoResume::Ask
    );
    assert!(load("[usage]\nauto_resume = \"always\"\n", None).is_err());
}
