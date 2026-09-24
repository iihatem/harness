use std::path::{Path, PathBuf};

/// All files under `root`, sorted. Honours `.gitignore` (even outside a git repository), includes hidden
/// files, and never descends into `.git`.
pub fn files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .map(|entry| entry.into_path())
        .collect();
    out.sort();
    out
}
