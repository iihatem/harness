use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use harness_core::permission::Mode;
use serde::Deserialize;

/// Wire protocol spoken by a configured provider. P4 adds `openai-responses` and `anthropic-messages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenaiChat,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

/// One `config.toml` file as written by the user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

/// The merged, effective configuration plus warnings about settings that were ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub model: Option<String>,
    pub mode: Option<Mode>,
    pub max_steps: Option<u32>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub warnings: Vec<String>,
}

/// Parses one config file. A missing file is `Ok(None)`; an invalid one is an error naming file and line.
pub fn parse_file(path: &Path) -> Result<Option<ConfigFile>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
}

/// Loads the global config, then the workspace's `.harness/config.toml`.
///
/// Until workspace trust exists (P2), the project file may only set `model`, `max_steps`, and narrowing
/// modes; widening settings are ignored with a warning.
pub fn load(global_file: &Path, workspace: &Path) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    if let Some(global) = parse_file(global_file)? {
        cfg.model = global.model;
        cfg.mode = global.mode;
        cfg.max_steps = global.max_steps;
        cfg.providers = global.providers;
    }
    let project_file = workspace.join(".harness").join("config.toml");
    if let Some(project) = parse_file(&project_file)? {
        if project.model.is_some() {
            cfg.model = project.model;
        }
        if project.max_steps.is_some() {
            cfg.max_steps = project.max_steps;
        }
        match project.mode {
            Some(mode) if mode.is_narrow() => cfg.mode = Some(mode),
            Some(mode) => cfg.warnings.push(format!(
                "{}: ignoring mode `{mode}` from untrusted project config",
                project_file.display()
            )),
            None => {}
        }
        if !project.providers.is_empty() {
            cfg.warnings.push(format!(
                "{}: ignoring provider definitions from untrusted project config",
                project_file.display()
            ));
        }
    }
    Ok(cfg)
}
