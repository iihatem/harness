use std::{path::PathBuf, sync::Arc};

use harness_config::{
    config::{self, Config},
    paths::Paths,
    trust::TrustStore,
};
use harness_core::redact::Redactor;
use harness_providers::{credentials::Credentials, registry::Secrets};

/// Everything a command needs about where it runs.
pub struct Setup {
    pub paths: Paths,
    pub config: Config,
    pub workspace: PathBuf,
    /// The workspaces the user trusts.
    pub trust: TrustStore,
    /// Stored API keys and sign-in tokens.
    pub credentials: Arc<Credentials>,
    /// The secrets nothing harness writes may hold: those in the environment from the start, and
    /// each key or token a provider is given.
    pub redactor: Arc<Redactor>,
}

impl Setup {
    /// API keys from the environment, then from the credential store.
    pub fn keys(&self) -> Keys<'_> {
        Keys {
            credentials: &self.credentials,
            redactor: &self.redactor,
        }
    }
}

/// API keys from the environment, then from the credential store (`harness auth add`).
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Arc<Credentials>,
    redactor: &'a Arc<Redactor>,
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

    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.credentials.clone())
    }

    fn redactor(&self) -> Option<Arc<Redactor>> {
        Some(self.redactor.clone())
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
    let credentials = Arc::new(Credentials::open(&paths.data_dir, env));
    let redactor = Arc::new(Redactor::default());
    redactor.add_env(std::env::vars());
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
        credentials,
        redactor,
    })
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
