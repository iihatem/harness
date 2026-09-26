//! Platform-neutral helpers for validating candidate writable roots
//! (`$TMPDIR`, per-user cache/temp dirs, ...) against the home directory.
//!
//! Shared by the macOS Seatbelt profile builder (`macos::profile`) and the
//! Linux Landlock ruleset builder (`linux::fs`): both need to reject a
//! candidate root that is `/`, `$HOME` itself, or an ancestor of `$HOME`,
//! since making any of those writable would defeat the sandbox.

use std::path::{Path, PathBuf};

/// The canonical `$HOME`, when it is set to an absolute path.
pub(crate) fn home_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !home.is_absolute() {
        return None;
    }
    Some(std::fs::canonicalize(&home).unwrap_or(home))
}

/// Canonicalizes a candidate temp/cache root taken from the environment or
/// `getconf`. Returns `None` when it is relative, does not exist, or is too
/// broad to make writable: `/`, `home`, or an ancestor of `home`.
pub(crate) fn safe_root(candidate: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if !candidate.is_absolute() {
        return None;
    }
    let canon = std::fs::canonicalize(candidate).ok()?;
    let too_broad = canon.parent().is_none() || home.is_some_and(|home| home.starts_with(&canon));
    (!too_broad).then_some(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    #[test]
    fn safe_root_rejects_root_home_and_its_ancestors() {
        let (_d, home) = canon_tempdir();
        let parent = home.parent().unwrap().to_path_buf();
        assert_eq!(safe_root(Path::new("/"), Some(&home)), None);
        assert_eq!(safe_root(Path::new("/"), None), None);
        assert_eq!(safe_root(&home, Some(&home)), None);
        assert_eq!(safe_root(&parent, Some(&home)), None);
        // Non-canonical spellings are canonicalized before the check.
        std::fs::create_dir(home.join("sub")).unwrap();
        assert_eq!(safe_root(&home.join("."), Some(&home)), None);
        assert_eq!(safe_root(&home.join("sub/.."), Some(&home)), None);
        assert_eq!(safe_root(&home.join("sub/../.."), Some(&home)), None);
    }

    #[test]
    fn safe_root_accepts_a_directory_inside_home_or_elsewhere() {
        let (_d, home) = canon_tempdir();
        let inside = home.join("tmp");
        std::fs::create_dir(&inside).unwrap();
        assert_eq!(safe_root(&inside, Some(&home)), Some(inside.clone()));
        let (_e, other) = canon_tempdir();
        assert_eq!(safe_root(&other, Some(&home)), Some(other.clone()));
        assert_eq!(safe_root(&other, None), Some(other));
    }

    #[test]
    fn safe_root_rejects_relative_and_missing_paths() {
        let (_d, home) = canon_tempdir();
        assert_eq!(safe_root(Path::new("."), Some(&home)), None);
        assert_eq!(safe_root(Path::new("tmp"), Some(&home)), None);
        assert_eq!(safe_root(&home.join("missing"), Some(&home)), None);
    }
}
