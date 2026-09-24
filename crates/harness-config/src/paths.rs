use std::path::PathBuf;

/// The three directories harness uses, per the XDG base-directory spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathsError {
    #[error("cannot determine the home directory: HOME is not set")]
    NoHome,
}

impl Paths {
    /// Resolves directories from environment variables. `HARNESS_HOME` overrides everything;
    /// relative XDG values are ignored, as the XDG spec requires.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Paths, PathsError> {
        if let Some(root) = get("HARNESS_HOME").filter(|v| !v.is_empty()) {
            let root = PathBuf::from(root);
            return Ok(Paths {
                config_dir: root.join("config"),
                data_dir: root.join("data"),
                state_dir: root.join("state"),
            });
        }
        let home = get("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
        let base = |var: &str, fallback: &str| -> Result<PathBuf, PathsError> {
            match get(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
                Some(dir) => Ok(dir.join("harness")),
                None => Ok(home
                    .clone()
                    .ok_or(PathsError::NoHome)?
                    .join(fallback)
                    .join("harness")),
            }
        };
        Ok(Paths {
            config_dir: base("XDG_CONFIG_HOME", ".config")?,
            data_dir: base("XDG_DATA_HOME", ".local/share")?,
            state_dir: base("XDG_STATE_HOME", ".local/state")?,
        })
    }

    pub fn from_process_env() -> Result<Paths, PathsError> {
        Self::from_env(|key| std::env::var(key).ok())
    }

    pub fn global_config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}
