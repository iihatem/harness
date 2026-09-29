use std::path::PathBuf;

use harness_config::{
    config::{self, Config},
    paths::Paths,
    trust::TrustStore,
};
use harness_providers::{credentials::Credentials, registry::Secrets};

/// Everything a command needs about where it runs.
pub struct Setup {
    pub paths: Paths,
    pub config: Config,
    pub workspace: PathBuf,
    /// The workspaces the user trusts.
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Credentials,
}

impl Setup {
    /// API keys from the environment, then from the credential store.
    pub fn keys(&self) -> Keys<'_> {
        Keys {
            credentials: &self.credentials,
        }
    }
}

/// API keys from the environment, then from the credential store (`harness auth add`).
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Credentials,
}

impl Secrets for Keys<'_> {
    fn env(&self, var: &str) -> Option<String> {
        env(var)
    }

    fn stored(&self, provider: &str) -> Option<String> {
        match self.credentials.active(provider) {
            Ok(key) => key,
            Err(e) => {
                eprintln!(
                    "warning: cannot read the stored credentials: {}",
                    crate::term::terminal_safe(&e.to_string())
                );
                None
            }
        }
    }
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
    let credentials = Credentials::open(&paths.data_dir, env);
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
        credentials,
    })
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
