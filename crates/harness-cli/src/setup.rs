use std::{path::PathBuf, sync::Arc};

use harness_config::{
    config::{self, Config},
    paths::Paths,
    trust::TrustStore,
};
use harness_core::redact::Redactor;
use harness_providers::{
    credentials::{CredentialError, Credentials},
    registry::Secrets,
};

/// Where environment variables are read: the process's own, or a test's.
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

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
    /// Where API keys and the test hooks are read from the environment.
    pub env: Env,
}

impl Setup {
    /// Prints, on stderr, what the credential store has had to warn about since the last call:
    /// that a credential went to the file, say, or that a renewed sign-in could not be stored.
    pub fn print_credential_warnings(&self) {
        for warning in self.credentials.take_warnings() {
            eprintln!("warning: {}", crate::term::terminal_safe(&warning));
        }
    }

    /// API keys from the environment, then from the credential store.
    pub fn keys(&self) -> Keys<'_> {
        Keys {
            credentials: &self.credentials,
            redactor: &self.redactor,
            env: &self.env,
        }
    }
}

/// API keys from the environment, then from the credential store (`harness auth add`).
#[derive(Clone, Copy)]
pub struct Keys<'a> {
    credentials: &'a Arc<Credentials>,
    redactor: &'a Arc<Redactor>,
    env: &'a Env,
}

impl Secrets for Keys<'_> {
    fn env(&self, var: &str) -> Option<String> {
        (self.env)(var)
    }

    fn profile(&self, provider: &str) -> Result<String, CredentialError> {
        self.credentials.active_profile(provider)
    }

    fn stored(&self, provider: &str) -> Result<Option<String>, CredentialError> {
        self.credentials.active(provider)
    }

    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(self.credentials.clone())
    }

    fn redactor(&self) -> Option<Arc<Redactor>> {
        Some(self.redactor.clone())
    }
}

/// Loads paths and configuration. Errors are user-facing messages (exit code 2), redacted.
///
/// The secrets harness knows before it reads the configuration (those in the environment and
/// the credential file) are registered first, so that what the configuration warns about, or why
/// it cannot be read, is printed without them; then the key variable of every configured
/// provider is.
pub fn load() -> Result<Setup, String> {
    let workspace = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .map_err(|e| format!("cannot determine the working directory: {e}"))?;
    let paths = Paths::from_process_env().map_err(|e| e.to_string())?;
    load_in(workspace, paths, Arc::new(env))
}

/// Loads the configuration for `workspace`, with `paths`, reading API keys, the credential
/// store's settings (`HARNESS_CREDENTIAL_STORE`) and the test hooks from `env`.
pub fn load_in(workspace: PathBuf, paths: Paths, env: Env) -> Result<Setup, String> {
    let store_env = env.clone();
    let credentials = Arc::new(Credentials::open(&paths.data_dir, move |var| {
        store_env(var)
    }));
    let redactor = Arc::new(known_secrets(&credentials));
    let redacted = |message: String| redactor.redact(&message);
    let trust = TrustStore::load(&paths.data_dir).map_err(|e| redacted(e.to_string()))?;
    let config = config::load(&paths.global_config_file(), &workspace, &trust)
        .map_err(|e| redacted(e.to_string()))?;
    register_key_variables(&redactor, &config);
    for warning in &config.warnings {
        eprintln!(
            "warning: {}",
            crate::term::terminal_safe(&redactor.redact(warning))
        );
    }
    Ok(Setup {
        paths,
        config,
        workspace,
        trust,
        credentials,
        redactor,
        env,
    })
}

/// The secrets known from the start of the run: those in the environment and those in the
/// credential file, whether or not this run uses them. The keychain is not read for this: a key
/// harness reads from it is registered when it is read.
pub fn known_secrets(credentials: &Credentials) -> Redactor {
    let redactor = Redactor::default();
    redactor.add_env(std::env::vars_os());
    for secret in credentials.file_secrets() {
        redactor.add(&secret);
    }
    redactor
}

/// Registers the key variable of every configured provider, whatever it is called.
fn register_key_variables(redactor: &Redactor, config: &Config) {
    for var in config
        .providers
        .values()
        .filter_map(|p| p.api_key_env.as_deref())
    {
        if let Some(value) = std::env::var_os(var) {
            redactor.add(&value.to_string_lossy());
        }
    }
}

pub fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}
