use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process,
};

use serde::{Deserialize, Serialize};

use crate::config::ConfigError;

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
}

/// Workspaces the user trusted, keyed by canonical path, each with the fingerprint of the widening
/// settings that were shown when trust was granted.
#[derive(Debug, Default)]
pub struct TrustStore {
    path: Option<PathBuf>,
    file: TrustFile,
}

impl TrustStore {
    /// Loads `data_dir/trust.toml`; a missing file is an empty store.
    pub fn load(data_dir: &Path) -> Result<TrustStore, ConfigError> {
        let path = data_dir.join("trust.toml");
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| ConfigError::Parse {
                path: path.clone(),
                message: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => TrustFile::default(),
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        Ok(TrustStore {
            path: Some(path),
            file,
        })
    }

    fn key(workspace: &Path) -> String {
        workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf())
            .display()
            .to_string()
    }

    pub fn is_trusted(&self, workspace: &Path, fingerprint: &str) -> bool {
        self.file
            .workspaces
            .get(&Self::key(workspace))
            .is_some_and(|trusted| trusted == fingerprint)
    }

    pub fn trust(&mut self, workspace: &Path, fingerprint: &str) -> Result<(), ConfigError> {
        self.file
            .workspaces
            .insert(Self::key(workspace), fingerprint.to_string());
        self.save()
    }

    /// Returns whether the workspace was trusted before.
    pub fn revoke(&mut self, workspace: &Path) -> Result<bool, ConfigError> {
        let removed = self.file.workspaces.remove(&Self::key(workspace)).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<(), ConfigError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let io = |source| ConfigError::Io {
            path: path.clone(),
            source,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let text = toml::to_string(&self.file).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let tmp_path = path.with_file_name(format!(
            "{}.tmp-{}",
            path.file_name().unwrap().to_string_lossy(),
            process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .map_err(io)?;
        file.write_all(text.as_bytes()).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        std::fs::rename(&tmp_path, path).map_err(io)
    }
}
