use std::path::PathBuf;

use harness_config::{
    config::{self, Config},
    paths::Paths,
    trust::TrustStore,
};

/// Everything a command needs about where it runs.
pub struct Setup {
    pub paths: Paths,
    pub config: Config,
    pub workspace: PathBuf,
    /// The workspaces the user trusts.
    pub trust: TrustStore,
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2).
pub fn load() -> Result<Setup, String> {
    let workspace = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    let paths = Paths::from_process_env().map_err(|e| e.to_string())?;
    let trust = TrustStore::load(&paths.data_dir).map_err(|e| e.to_string())?;
    let config =
        config::load(&paths.global_config_file(), &workspace, &trust).map_err(|e| e.to_string())?;
    for warning in &config.warnings {
        eprintln!("warning: {}", crate::term::terminal_safe(warning));
    }
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
    })
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
