//! Finds every gitdir in a workspace.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::linked::{follow, linked_gitdirs_at, pointer, within};

/// How many directories below `modules/` a submodule's gitdir is looked for:
/// a submodule's name can contain `/`.
const MAX_MODULE_DEPTH: usize = 8;

/// Where git keeps metadata in one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitIndex {
    /// Every `.git` entry (directory, gitfile or symlink) in the workspace
    /// outside git-ignored directories, the top-level one included.
    pub dot_gits: BTreeSet<PathBuf>,
    /// Every gitdir inside the workspace: each `.git` directory; the gitdir a
    /// gitfile or symlinked `.git` leads to and the one its `commondir` names;
    /// and in each, the gitdirs of linked worktrees (`worktrees/*`) and
    /// submodules (`modules/**`). A parent sorts before its children.
    pub gitdirs: BTreeSet<PathBuf>,
    /// Entries inside the workspace on the way from a gitfile or symlinked
    /// `.git` to its gitdirs: each symlink, directory and gitfile.
    pub links: BTreeSet<PathBuf>,
}

/// Indexes the canonical `workspace`. Directories git ignores are not
/// walked, nor is `skip` (harness's quarantine directory, should it be inside
/// the workspace), and symlinks are not followed.
pub fn discover(workspace: &Path, skip: Option<&Path>) -> GitIndex {
    let mut index = GitIndex::default();
    for holder in holders(workspace, skip) {
        let dot_git = holder.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            continue;
        };
        index.dot_gits.insert(dot_git.clone());
        if meta.is_dir() {
            index.gitdirs.insert(dot_git);
        } else {
            let linked = linked_gitdirs_at(&holder, workspace);
            index.gitdirs.extend(linked.gitdirs);
            index.links.extend(linked.entries);
        }
    }
    let mut pending: Vec<PathBuf> = index.gitdirs.iter().cloned().collect();
    while let Some(gitdir) = pending.pop() {
        let common = pointer(&gitdir.join("commondir"), b"")
            .and_then(|common| follow(&gitdir.join(common), &mut Vec::new()));
        for found in common.into_iter().chain(nested_gitdirs(&gitdir)) {
            if within(&found, workspace) && index.gitdirs.insert(found.clone()) {
                pending.push(found);
            }
        }
    }
    index
}

/// The gitdirs of `gitdir`'s linked worktrees (every directory in
/// `worktrees/`) and submodules (every directory below `modules/` that holds
/// a `HEAD`). Symlinks are not followed.
pub(crate) fn nested_gitdirs(gitdir: &Path) -> Vec<PathBuf> {
    let mut found = subdirs(&gitdir.join("worktrees"));
    module_gitdirs(&gitdir.join("modules"), 0, &mut found);
    found
}

fn module_gitdirs(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth >= MAX_MODULE_DEPTH {
        return;
    }
    for sub in subdirs(dir) {
        if std::fs::symlink_metadata(sub.join("HEAD")).is_ok() {
            found.push(sub);
        } else {
            module_gitdirs(&sub, depth + 1, found);
        }
    }
}

/// The directories directly in `dir`, when `dir` is itself a directory (not
/// a symlink to one).
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// The workspace and every directory below it that git does not ignore,
/// except `.git` directories, what is below them, and `skip`.
fn holders(workspace: &Path, skip: Option<&Path>) -> Vec<PathBuf> {
    let skip = skip.map(Path::to_path_buf);
    ignore::WalkBuilder::new(workspace)
        .hidden(false)
        .ignore(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            entry.file_name() != ".git" && skip.as_deref() != Some(entry.path())
        })
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_dir()))
        .map(ignore::DirEntry::into_path)
        .collect()
}
