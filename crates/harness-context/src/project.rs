//! Where a project starts: the repository root, the root of instruction discovery, and the key
//! that names a project's sessions and checkpoints.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The nearest directory at or above `dir` that holds a `.git` entry git would follow (a
/// directory, a gitfile, or a symlink to either), or `None` outside a repository.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Where instruction discovery stops walking up from `dir`: the repository root; outside a
/// repository, `home` when `dir` is inside it, otherwise `dir` itself.
pub fn discovery_root(dir: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(root) = repo_root(dir) {
        return root;
    }
    match home {
        Some(home) if dir.starts_with(home) => home.to_path_buf(),
        _ => dir.to_path_buf(),
    }
}

/// The directory a project's sessions and checkpoints belong to: the repository root, or `dir`
/// outside a repository.
pub fn project_root(dir: &Path) -> PathBuf {
    repo_root(dir).unwrap_or_else(|| dir.to_path_buf())
}

/// A directory name for the project at `root`: its last component, made filename-safe and cut
/// to 40 characters, then `-` and the first 16 hex digits of the SHA-256 of the whole path.
pub fn project_key(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let name = name.trim_start_matches('.');
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let hash = &hex::encode(digest)[..16];
    if name.is_empty() {
        format!("root-{hash}")
    } else {
        format!("{name}-{hash}")
    }
}
