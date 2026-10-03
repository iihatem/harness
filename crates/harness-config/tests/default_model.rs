//! The first-run model choice is saved as `model` in the global `config.toml`, keeping what the
//! file already says.

use harness_config::{config, trust::TrustStore};

fn load_model(global: &std::path::Path) -> Option<String> {
    let ws = tempfile::tempdir().unwrap();
    let trust = TrustStore::load(ws.path()).unwrap();
    config::load(global, ws.path(), &trust).unwrap().model
}

#[test]
fn a_missing_config_file_is_created_with_the_model() {
    let home = tempfile::tempdir().unwrap();
    let global = home.path().join("config/harness/config.toml");
    config::save_default_model(&global, "ollama/qwen3-coder:30b").unwrap();
    assert_eq!(
        std::fs::read_to_string(&global).unwrap(),
        "model = \"ollama/qwen3-coder:30b\"\n"
    );
    assert_eq!(
        load_model(&global).as_deref(),
        Some("ollama/qwen3-coder:30b")
    );
}

#[test]
fn the_model_goes_first_and_the_rest_is_kept_as_written() {
    let home = tempfile::tempdir().unwrap();
    let global = home.path().join("config.toml");
    let before = "# my settings\nmode = \"ask\"\n\n[providers.mock]\nprotocol = \"openai-chat\" # local\nbase_url = \"http://127.0.0.1:9/v1\"\n";
    std::fs::write(&global, before).unwrap();
    config::save_default_model(&global, "mock/coder").unwrap();
    let after = std::fs::read_to_string(&global).unwrap();
    assert_eq!(after, format!("model = \"mock/coder\"\n{before}"));
    assert_eq!(load_model(&global).as_deref(), Some("mock/coder"));
}

#[test]
fn an_id_is_written_as_a_toml_string() {
    let home = tempfile::tempdir().unwrap();
    let global = home.path().join("config.toml");
    config::save_default_model(&global, "odd/a\"b\\c").unwrap();
    assert_eq!(load_model(&global).as_deref(), Some("odd/a\"b\\c"));
}

#[test]
fn a_file_that_sets_a_model_already_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let global = home.path().join("config.toml");
    std::fs::write(&global, "model = \"ollama/llama3\"\n").unwrap();
    let error = config::save_default_model(&global, "mock/coder").unwrap_err();
    assert!(error.to_string().contains("already sets"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&global).unwrap(),
        "model = \"ollama/llama3\"\n"
    );
}

#[test]
fn a_file_that_is_not_toml_is_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let global = home.path().join("config.toml");
    std::fs::write(&global, "model = [\n").unwrap();
    assert!(config::save_default_model(&global, "mock/coder").is_err());
    assert_eq!(std::fs::read_to_string(&global).unwrap(), "model = [\n");
}

#[test]
fn the_file_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let global = home.path().join("config.toml");
    std::fs::write(&global, "mode = \"ask\"\n").unwrap();
    std::fs::set_permissions(&global, std::fs::Permissions::from_mode(0o600)).unwrap();
    config::save_default_model(&global, "mock/coder").unwrap();
    let mode = std::fs::metadata(&global).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

// Review B M1: a config file made now is private (0600) and so is the directory made for it
// (0700); an existing directory is left as it is.
#[test]
fn a_new_file_is_private_and_so_is_the_directory_made_for_it() {
    use std::os::unix::fs::PermissionsExt;
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let home = tempfile::tempdir().unwrap();
    let existing = home.path().join("kept");
    std::fs::create_dir(&existing).unwrap();
    std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
    let global = existing.join("config/harness/config.toml");
    config::save_default_model(&global, "mock/coder").unwrap();
    assert_eq!(mode(&global), 0o600);
    assert_eq!(mode(global.parent().unwrap()), 0o700);
    assert_eq!(mode(global.parent().unwrap().parent().unwrap()), 0o700);
    assert_eq!(mode(&existing), 0o755);
}

// Review B M2: a config file that is a link is written through: the link stays, and the file it
// names changes.
#[test]
fn a_config_file_that_is_a_link_is_written_through() {
    let home = tempfile::tempdir().unwrap();
    let dotfiles = home.path().join("dotfiles");
    std::fs::create_dir(&dotfiles).unwrap();
    let target = dotfiles.join("harness.toml");
    std::fs::write(&target, "mode = \"ask\"\n").unwrap();
    let global = home.path().join("config.toml");
    std::os::unix::fs::symlink("dotfiles/harness.toml", &global).unwrap();
    config::save_default_model(&global, "mock/coder").unwrap();
    assert!(
        std::fs::symlink_metadata(&global)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "model = \"mock/coder\"\nmode = \"ask\"\n"
    );
    let left: Vec<_> = std::fs::read_dir(&dotfiles).unwrap().collect();
    assert_eq!(left.len(), 1, "no temporary file is left behind");
}
