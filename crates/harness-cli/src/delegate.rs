//! `claude-code/*` and `codex/*` as the CLI sees them: the settings they run with, where the
//! Claude subscription answer is kept, and which of their models can be listed.

use std::{path::PathBuf, sync::Arc};

use harness_delegate::{
    BackendKind,
    models::{self, ListOptions},
};
use harness_providers::delegated::{CLAUDE_CODE, CODEX, DelegateSettings, OptInStore};

use crate::setup::Setup;

/// The answer to "use your Claude subscription?", kept in the data directory (not in the user's
/// config file, which harness never edits). Written readable by the user only.
pub struct FileOptIn {
    path: PathBuf,
}

impl FileOptIn {
    pub fn new(data_dir: &std::path::Path) -> FileOptIn {
        FileOptIn {
            path: data_dir.join("delegate.json"),
        }
    }
}

impl OptInStore for FileOptIn {
    fn get(&self) -> Option<bool> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str::<serde_json::Value>(&text).ok()?["claude_subscription"].as_bool()
    }

    fn set(&self, yes: bool) -> Result<(), String> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.path)?;
            file.write_all(format!("{{\"claude_subscription\":{yes}}}\n").as_bytes())
        };
        write().map_err(|e| format!("cannot keep the answer in {}: {e}", self.path.display()))
    }
}

/// What the registry needs from the user's configuration and stored answer. The configuration's
/// value wins over what was answered at the first use.
pub fn settings(setup: &Setup) -> DelegateSettings {
    let store = Arc::new(FileOptIn::new(&setup.paths.data_dir));
    DelegateSettings {
        claude_subscription: setup
            .config
            .delegate
            .claude_subscription
            .or_else(|| store.get()),
        store: Some(store),
        allow_untested: setup.config.delegate.allow_untested,
        ..DelegateSettings::default()
    }
}

/// The delegated models that can be used now, as ids, and why the ones that cannot are missing
/// (`(provider, reason)`).
pub async fn listing(setup: &Setup) -> (Vec<String>, Vec<(&'static str, String)>) {
    let options = ListOptions {
        // The test hooks of a debug build, so a fake program can stand in.
        binary: None,
        allow_untested: setup.config.delegate.allow_untested,
        ..ListOptions::default()
    };
    let hook = |var: &str| {
        harness_providers::registry::test_hook(var, |v| (setup.env)(v)).map(PathBuf::from)
    };
    let for_claude = ListOptions {
        binary: hook("HARNESS_CLAUDE_BIN"),
        ..options.clone()
    };
    let for_codex = ListOptions {
        binary: hook("HARNESS_CODEX_BIN"),
        ..options
    };
    let (claude, codex) = tokio::join!(
        models::claude_aliases(&for_claude),
        models::codex_models(&for_codex)
    );
    let mut ids = Vec::new();
    let mut missing = Vec::new();
    for (kind, provider, result) in [
        (BackendKind::Claude, CLAUDE_CODE, claude),
        (BackendKind::Codex, CODEX, codex),
    ] {
        match result {
            Ok(list) if list.is_empty() => {
                missing.push((provider, format!("{} listed no models", kind.binary())))
            }
            Ok(list) => ids.extend(list.into_iter().map(|m| format!("{provider}/{m}"))),
            Err(e) => missing.push((provider, e.to_string())),
        }
    }
    (ids, missing)
}

/// Whether the program a delegated model `id` runs in can be used now (found, and a version
/// harness was tested with): `Err` says why. `Ok` for any other model.
pub async fn ready(setup: &Setup, id: &str) -> Result<(), String> {
    let (provider, _) = id.split_once('/').unwrap_or((id, ""));
    let (kind, var) = match provider {
        CLAUDE_CODE => (BackendKind::Claude, "HARNESS_CLAUDE_BIN"),
        CODEX => (BackendKind::Codex, "HARNESS_CODEX_BIN"),
        _ => return Ok(()),
    };
    let options = harness_delegate::detect::DetectOptions {
        binary: harness_providers::registry::test_hook(var, |v| (setup.env)(v)).map(PathBuf::from),
        allow_untested: setup.config.delegate.allow_untested,
        ..Default::default()
    };
    harness_delegate::detect::detect(kind, &options)
        .await
        .map(|_| ())
        .map_err(|e| format!("{provider} cannot be used: {e}"))
}
