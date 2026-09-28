use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use harness_core::{
    agent::DEFAULT_MAX_STEPS,
    compaction::{DEFAULT_KEEP_RECENT, DEFAULT_THRESHOLD},
    permission::Mode,
};
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

/// `sandbox.linux_git_protection`: what to do on Linux when user namespaces are unavailable, so
/// git metadata is protected only after each command (the basic tier).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinuxGitProtection {
    /// Run commands in the basic tier after a startup warning.
    #[default]
    BestEffort,
    /// Treat the basic tier as no sandbox: every shell command asks first.
    Required,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub writable_roots: Vec<String>,
    pub allow_localhost: Option<bool>,
    /// `sandbox.linux_git_protection`: see [`LinuxGitProtection`]; unset means `"best-effort"`.
    /// A project's `"required"` always applies; a project's `"best-effort"` over a global
    /// `"required"` widens it, so it applies only once the workspace is trusted.
    pub linux_git_protection: Option<LinuxGitProtection>,
}

/// The lowest compaction threshold, in percent, a project may set without workspace trust: below
/// it harness would summarize, a paid request that replaces verbatim context, every few turns.
pub const MIN_UNTRUSTED_THRESHOLD_PERCENT: u8 = 50;

/// `[compaction]`: when the conversation is summarized, in percent of the context window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionSettings {
    /// Compact when estimated usage reaches this share of the context window (1 to 100).
    pub threshold_percent: Option<u8>,
    /// Keep this share of the context window of recent messages as they are (below the
    /// threshold).
    pub keep_recent_percent: Option<u8>,
}

impl CompactionSettings {
    /// The threshold as a fraction of the context window.
    pub fn threshold(&self) -> f64 {
        self.threshold_percent
            .map_or(DEFAULT_THRESHOLD, |p| f64::from(p) / 100.0)
    }

    /// The share kept as recent messages, as a fraction of the context window.
    pub fn keep_recent(&self) -> f64 {
        self.keep_recent_percent
            .map_or(DEFAULT_KEEP_RECENT, |p| f64::from(p) / 100.0)
    }

    /// Which value is outside 1 to 100, if any.
    fn out_of_range(&self) -> Option<String> {
        let bad = |p: Option<u8>| p.is_some_and(|p| p == 0 || p > 100);
        if bad(self.threshold_percent) {
            return Some("compaction.threshold_percent must be between 1 and 100".into());
        }
        if bad(self.keep_recent_percent) {
            return Some("compaction.keep_recent_percent must be between 1 and 100".into());
        }
        None
    }

    /// These settings with `project`'s over them.
    fn overlaid(&self, project: &CompactionSettings) -> CompactionSettings {
        CompactionSettings {
            threshold_percent: project.threshold_percent.or(self.threshold_percent),
            keep_recent_percent: project.keep_recent_percent.or(self.keep_recent_percent),
        }
    }

    /// What is wrong with these settings, if anything.
    fn problem(&self) -> Option<String> {
        if let Some(problem) = self.out_of_range() {
            return Some(problem);
        }
        if self.keep_recent() >= self.threshold() {
            return Some(
                "compaction.keep_recent_percent must be below compaction.threshold_percent".into(),
            );
        }
        None
    }
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
    #[serde(default)]
    pub compaction: CompactionSettings,
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
    pub compaction: CompactionSettings,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub confirm: Vec<String>,
    pub read_dirs: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub allow_localhost: bool,
    pub linux_git_protection: LinuxGitProtection,
    /// Whether the user trusted this workspace with its project settings as they are now
    /// (`harness trust`), so that their widening settings apply. A workspace with no such
    /// settings can be trusted too. A project command file's `model` applies only then.
    pub trusted: bool,
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
/// fingerprint, that of the empty set included, so any change to these settings needs trust again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widening {
    /// Empty when the project has no widening settings.
    pub items: Vec<String>,
    pub fingerprint: String,
}

/// The mode, step limit and Linux git protection in effect without the project config: the global
/// config's, or the defaults. A project setting that does not go beyond them narrows and needs no
/// trust.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    mode: Mode,
    max_steps: u32,
    linux_git_protection: LinuxGitProtection,
}

impl Baseline {
    fn new(global: Option<&ConfigFile>, workspace: &Path) -> Baseline {
        Baseline {
            mode: global
                .and_then(|g| g.mode)
                .unwrap_or_else(|| default_mode(workspace)),
            max_steps: global
                .and_then(|g| g.max_steps)
                .unwrap_or(DEFAULT_MAX_STEPS),
            linux_git_protection: global
                .and_then(|g| g.sandbox.linux_git_protection)
                .unwrap_or_default(),
        }
    }
}

/// `auto` inside a git work tree (changes are recoverable), `ask` elsewhere.
pub fn default_mode(workspace: &Path) -> Mode {
    if workspace.ancestors().any(|dir| dir.join(".git").exists()) {
        Mode::Auto
    } else {
        Mode::Ask
    }
}

fn widening(project: &ConfigFile, baseline: Baseline) -> Widening {
    let mut items = Vec::new();
    if let Some(mode) = project.mode.filter(|m| !m.grants_at_most(baseline.mode)) {
        items.push(format!("mode = \"{mode}\""));
    }
    if let Some(steps) = project.max_steps.filter(|&n| n > baseline.max_steps) {
        items.push(format!("max_steps = {steps}"));
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
            "providers.{name:?}: protocol = {:?}, base_url = {:?}, api_key_env = {:?}",
            provider.protocol, provider.base_url, provider.api_key_env
        ));
    }
    for root in &project.sandbox.writable_roots {
        items.push(format!("sandbox.writable_roots: {root:?}"));
    }
    if let Some(true) = project.sandbox.allow_localhost {
        items.push("sandbox.allow_localhost = true".to_string());
    }
    if project.sandbox.linux_git_protection == Some(LinuxGitProtection::BestEffort)
        && baseline.linux_git_protection == LinuxGitProtection::Required
    {
        items.push("sandbox.linux_git_protection = \"best-effort\"".to_string());
    }
    // It does not widen what the agent may do, but it needs trust all the same (ruling P3-R4).
    if let Some(p) = low_threshold(project) {
        items.push(format!("{THRESHOLD_ITEM}{p}"));
    }
    // No item is empty, so only the empty set joins to "".
    let fingerprint = hex::encode(Sha256::digest(items.join("\n").as_bytes()));
    Widening { items, fingerprint }
}

/// How a project's too-low compaction threshold is listed among the settings that need trust.
const THRESHOLD_ITEM: &str = "compaction.threshold_percent = ";

/// The project's compaction threshold when it is below [`MIN_UNTRUSTED_THRESHOLD_PERCENT`].
fn low_threshold(project: &ConfigFile) -> Option<u8> {
    project
        .compaction
        .threshold_percent
        .filter(|&p| p < MIN_UNTRUSTED_THRESHOLD_PERCENT)
}

pub fn project_file(workspace: &Path) -> PathBuf {
    workspace.join(".harness").join("config.toml")
}

/// The widening settings in the workspace's project config, possibly none (shown and trusted by
/// `harness trust`). Whether a mode or step limit widens depends on the global config, so it is
/// read too.
pub fn project_widening(global_file: &Path, workspace: &Path) -> Result<Widening, ConfigError> {
    let global = parse_file(global_file)?;
    let baseline = Baseline::new(global.as_ref(), workspace);
    let path = project_file(workspace);
    let project = parse_file(&path)?.unwrap_or_default();
    // Settings that would be invalid once trusted cannot be trusted.
    let global_compaction = global.map(|g| g.compaction).unwrap_or_default();
    if let Some(message) = project
        .compaction
        .out_of_range()
        .or_else(|| global_compaction.overlaid(&project.compaction).problem())
    {
        return Err(ConfigError::Parse { path, message });
    }
    Ok(widening(&project, baseline))
}

/// Loads the global config, then the workspace's `.harness/config.toml`. Project settings that narrow
/// what the agent may do always apply; widening ones apply only when `trust` holds their fingerprint,
/// which also makes the workspace trusted (`Config::trusted`) when it has none.
pub fn load(
    global_file: &Path,
    workspace: &Path,
    trust: &TrustStore,
) -> Result<Config, ConfigError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home = home.as_deref();
    let mut cfg = Config::default();
    let global = parse_file(global_file)?;
    let baseline = Baseline::new(global.as_ref(), workspace);
    if let Some(message) = global.as_ref().and_then(|g| g.compaction.problem()) {
        return Err(ConfigError::Parse {
            path: global_file.to_path_buf(),
            message,
        });
    }
    if let Some(global) = global {
        let base = global_file.parent().unwrap_or(Path::new("/"));
        cfg.model = global.model;
        cfg.mode = global.mode;
        cfg.max_steps = global.max_steps;
        cfg.providers = global.providers;
        cfg.allow = global.permissions.allow;
        cfg.deny = global.permissions.deny;
        cfg.confirm = global.permissions.confirm;
        cfg.compaction = global.compaction;
        cfg.read_dirs = expand_all(&global.permissions.read_dirs, base, home);
        cfg.writable_roots = expand_all(&global.sandbox.writable_roots, base, home);
        cfg.allow_localhost = global.sandbox.allow_localhost.unwrap_or(false);
        cfg.linux_git_protection = global.sandbox.linux_git_protection.unwrap_or_default();
    }
    let path = project_file(workspace);
    let project = parse_file(&path)?;
    let widening = widening(project.as_ref().unwrap_or(&ConfigFile::default()), baseline);
    cfg.trusted = trust.is_trusted(workspace, &widening.fingerprint);
    if let Some(project) = project {
        if let Some(message) = project.compaction.out_of_range() {
            return Err(ConfigError::Parse { path, message });
        }
        cfg.deny.extend(project.permissions.deny.iter().cloned());
        cfg.confirm
            .extend(project.permissions.confirm.iter().cloned());
        // Compaction settings change when the conversation is summarized, not what the agent
        // may do, so they apply without trust, except a threshold low enough to summarize (a paid
        // request that drops verbatim context) every few turns (ruling P3-R4).
        // The project's settings are checked as they would apply once trusted, so a config that
        // is invalid then is invalid now too.
        let as_trusted = cfg.compaction.overlaid(&project.compaction);
        if let Some(message) = as_trusted.problem() {
            return Err(ConfigError::Parse { path, message });
        }
        match low_threshold(&project).filter(|_| !cfg.trusted) {
            None => cfg.compaction = as_trusted,
            Some(p) => {
                // The global threshold or the default applies, and the project's keep share
                // with it only while it is below that threshold.
                let with_keep = CompactionSettings {
                    threshold_percent: cfg.compaction.threshold_percent,
                    ..as_trusted
                };
                let mut ignored = format!("{THRESHOLD_ITEM}{p}");
                if with_keep.problem().is_none() {
                    cfg.compaction = with_keep;
                } else if let Some(keep) = project.compaction.keep_recent_percent {
                    ignored.push_str(&format!(" and keep_recent_percent = {keep}"));
                }
                cfg.warnings.push(format!(
                    "{}: ignoring {ignored}: a project may compact below {MIN_UNTRUSTED_THRESHOLD_PERCENT}% of the context window only in a trusted workspace, so {}% applies, keeping {}%; run `harness trust` to review and apply it",
                    path.display(),
                    (cfg.compaction.threshold() * 100.0).round(),
                    (cfg.compaction.keep_recent() * 100.0).round()
                ));
            }
        }
        if let Some(steps) = project.max_steps.filter(|&n| n <= baseline.max_steps) {
            cfg.max_steps = Some(steps);
        }
        if let Some(mode) = project.mode.filter(|m| m.grants_at_most(baseline.mode)) {
            cfg.mode = Some(mode);
        }
        if let Some(false) = project.sandbox.allow_localhost {
            cfg.allow_localhost = false;
        }
        if let Some(LinuxGitProtection::Required) = project.sandbox.linux_git_protection {
            cfg.linux_git_protection = LinuxGitProtection::Required;
        }
        // The threshold has its own warning above.
        let widening_items: Vec<&String> = widening
            .items
            .iter()
            .filter(|item| !item.starts_with(THRESHOLD_ITEM))
            .collect();
        if !widening_items.is_empty() {
            if cfg.trusted {
                if project.mode.is_some() {
                    cfg.mode = project.mode;
                }
                if project.max_steps.is_some() {
                    cfg.max_steps = project.max_steps;
                }
                if project.model.is_some() {
                    cfg.model = project.model.clone();
                }
                cfg.allow.extend(project.permissions.allow.iter().cloned());
                cfg.read_dirs
                    .extend(expand_all(&project.permissions.read_dirs, workspace, home));
                cfg.providers.extend(project.providers.clone());
                cfg.writable_roots.extend(expand_all(
                    &project.sandbox.writable_roots,
                    workspace,
                    home,
                ));
                if let Some(allow) = project.sandbox.allow_localhost {
                    cfg.allow_localhost = allow;
                }
                if let Some(protection) = project.sandbox.linux_git_protection {
                    cfg.linux_git_protection = protection;
                }
            } else {
                let items: Vec<&str> = widening_items.iter().map(|item| item.as_str()).collect();
                cfg.warnings.push(format!(
                    "{}: ignoring {} setting(s) that widen what the agent may do ({}); run `harness trust` to review and apply them",
                    path.display(),
                    items.len(),
                    items.join("; ")
                ));
            }
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
