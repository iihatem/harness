use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use harness_core::permission::Mode;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::trust::TrustStore;

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub confirm: Vec<String>,
    #[serde(default)]
    pub read_dirs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub allow_localhost: Option<bool>,
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
    #[serde(default)]
    pub permissions: PermissionsConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
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
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub confirm: Vec<String>,
    pub read_dirs: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
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

/// Project settings that widen what the agent may do, and a fingerprint of them. Trust is granted to a
/// fingerprint, so any change to these settings needs trust again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widening {
    pub items: Vec<String>,
    pub fingerprint: String,
}

pub fn widening(project: &ConfigFile) -> Option<Widening> {
    let mut items = Vec::new();
    if let Some(mode) = project.mode.filter(|m| !m.is_narrow()) {
        items.push(format!("mode = {mode:?}"));
    }
    if let Some(model) = &project.model {
        items.push(format!("model = {model:?}"));
    }
    for rule in &project.permissions.allow {
        items.push(format!("permissions.allow: {rule:?}"));
    }
    for dir in &project.permissions.read_dirs {
        items.push(format!("permissions.read_dirs: {dir:?}"));
    }
    for (name, provider) in &project.providers {
        items.push(format!(
            "providers.{name}: protocol = {:?}, base_url = {:?}, api_key_env = {:?}",
            provider.protocol, provider.base_url, provider.api_key_env
        ));
    }
    for root in &project.sandbox.writable_roots {
        items.push(format!("sandbox.writable_roots: {root:?}"));
    }
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    if items.is_empty() {
        return None;
    }
    let fingerprint = hex::encode(Sha256::digest(items.join("\n").as_bytes()));
    Some(Widening { items, fingerprint })
}

pub fn project_file(workspace: &Path) -> PathBuf {
    workspace.join(".harness").join("config.toml")
}

/// The widening settings in the workspace's project config, if any (shown by `harness trust`).
pub fn project_widening(workspace: &Path) -> Result<Option<Widening>, ConfigError> {
    Ok(parse_file(&project_file(workspace))?
        .as_ref()
        .and_then(widening))
}

/// Loads the global config, then the workspace's `.harness/config.toml`. Project settings that narrow
/// what the agent may do always apply; widening ones apply only when `trust` holds their fingerprint.
pub fn load(
    global_file: &Path,
    workspace: &Path,
    trust: &TrustStore,
) -> Result<Config, ConfigError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home = home.as_deref();
    let mut cfg = Config::default();
    if let Some(global) = parse_file(global_file)? {
        let base = global_file.parent().unwrap_or(Path::new("/"));
        cfg.model = global.model;
        cfg.mode = global.mode;
        cfg.max_steps = global.max_steps;
        cfg.providers = global.providers;
        cfg.allow = global.permissions.allow;
        cfg.deny = global.permissions.deny;
        cfg.confirm = global.permissions.confirm;
        cfg.read_dirs = expand_all(&global.permissions.read_dirs, base, home);
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
    }
    let path = project_file(workspace);
    if let Some(project) = parse_file(&path)? {
        cfg.deny.extend(project.permissions.deny.iter().cloned());
        cfg.confirm
            .extend(project.permissions.confirm.iter().cloned());
        if project.max_steps.is_some() {
            cfg.max_steps = project.max_steps;
        }
        if let Some(mode) = project.mode.filter(|m| m.is_narrow()) {
            cfg.mode = Some(mode);
        }
        if let Some(false) = project.sandbox.allow_localhost {
            cfg.allow_localhost = false;
        }
        match widening(&project) {
            None => {}
            Some(w) if trust.is_trusted(workspace, &w.fingerprint) => {
                if let Some(mode) = project.mode.filter(|m| !m.is_narrow()) {
                    cfg.mode = Some(mode);
                }
                if project.model.is_some() {
                    cfg.model = project.model.clone();
                }
                cfg.allow.extend(project.permissions.allow.iter().cloned());
                cfg.read_dirs
                    .extend(expand_all(&project.permissions.read_dirs, workspace, home));
                cfg.providers.extend(project.providers.clone());
                cfg.writable_roots
                    .extend(expand_all(&project.sandbox.writable_roots, workspace, home));
                if let Some(allow) = project.sandbox.allow_localhost {
                    cfg.allow_localhost = allow;
                }
            }
            Some(w) => cfg.warnings.push(format!(
                "{}: ignoring {} setting(s) that widen what the agent may do ({}); run `harness trust` to review and apply them",
                path.display(),
                w.items.len(),
                w.items.join("; ")
            )),
        }
    }
    Ok(cfg)
}

fn expand_all(paths: &[String], base: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    paths.iter().map(|p| expand(p, base, home)).collect()
}

/// `~` and `~/x` expand to the home directory; other relative paths are relative to `base`.
fn expand(path: &str, base: &Path, home: Option<&Path>) -> PathBuf {
    if let (Some(rest), Some(home)) = (path.strip_prefix('~'), home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return home.join(rest.trim_start_matches('/'));
    }
    if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_expand_home_and_relative_to_the_base() {
        let home = Path::new("/home/u");
        let base = Path::new("/work/proj");
        assert_eq!(expand("~", base, Some(home)), PathBuf::from("/home/u"));
        assert_eq!(
            expand("~/.cargo", base, Some(home)),
            PathBuf::from("/home/u/.cargo")
        );
        assert_eq!(
            expand("~other/x", base, Some(home)),
            PathBuf::from("/work/proj/~other/x")
        );
        assert_eq!(expand("/opt/x", base, Some(home)), PathBuf::from("/opt/x"));
        assert_eq!(
            expand("cache", base, Some(home)),
            PathBuf::from("/work/proj/cache")
        );
    }
}
