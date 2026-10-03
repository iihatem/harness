//! `edit_format` in a model profile.

use harness_config::{config, trust::TrustStore};
use harness_core::edit_format::EditFormat;

fn load(global: &str, project: &str) -> Result<config::Config, config::ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(&global_file, global).unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
    let trust = TrustStore::load(&dir.path().join("data")).unwrap();
    config::load(&global_file, &ws, &trust)
}

#[test]
fn a_profile_sets_the_format() {
    let cfg = load(
        "[profiles.\"ollama/qwen3-coder*\"]\nedit_format = \"apply_patch\"\n",
        "",
    )
    .unwrap();
    assert_eq!(
        cfg.profiles["ollama/qwen3-coder*"].edit_format,
        Some(EditFormat::ApplyPatch)
    );
}

#[test]
fn another_format_name_is_an_error_naming_the_file() {
    let error = load("[profiles.\"x/*\"]\nedit_format = \"diff\"\n", "")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("config.toml") && error.contains("line 2") && error.contains("apply_patch"),
        "{error}"
    );
}

// A project's profiles need trust, and the format is listed among what they set.
#[test]
fn a_projects_format_needs_trust_and_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("none.toml");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "[profiles.\"openai/*\"]\nedit_format = \"whole_file\"\n",
    )
    .unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    let cfg = config::load(&global_file, &ws, &trust).unwrap();
    assert!(cfg.profiles.is_empty());
    let widening = config::project_widening(&global_file, &ws).unwrap();
    assert_eq!(
        widening.items,
        ["profiles.\"openai/*\": edit_format = \"whole_file\""]
    );
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let cfg = config::load(&global_file, &ws, &trust).unwrap();
    assert_eq!(
        cfg.profiles["openai/*"].edit_format,
        Some(EditFormat::WholeFile)
    );
}

#[test]
fn a_trusted_project_goes_over_the_global_profile_field_by_field() {
    let dir = tempfile::tempdir().unwrap();
    let global_file = dir.path().join("config.toml");
    std::fs::write(
        &global_file,
        "[profiles.\"x/*\"]\nedit_format = \"hashline\"\ntemperature = 0.5\n",
    )
    .unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(ws.join(".harness")).unwrap();
    std::fs::write(
        ws.join(".harness/config.toml"),
        "[profiles.\"x/*\"]\ntemperature = 0.1\n",
    )
    .unwrap();
    let mut trust = TrustStore::load(&dir.path().join("data")).unwrap();
    let widening = config::project_widening(&global_file, &ws).unwrap();
    trust.trust(&ws, &widening.fingerprint).unwrap();
    let merged = &config::load(&global_file, &ws, &trust).unwrap().profiles["x/*"];
    assert_eq!(merged.edit_format, Some(EditFormat::Hashline));
    assert_eq!(merged.temperature, Some(0.1));
}
