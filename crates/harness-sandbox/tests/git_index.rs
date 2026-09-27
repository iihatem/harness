//! `gitmeta::discover` on real directory trees. Platform-neutral: runs on macOS and Linux.

use std::collections::BTreeSet;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use harness_sandbox::gitmeta::discover;

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canon = dir.path().canonicalize().unwrap();
    (dir, canon)
}

/// A minimal gitdir: git itself would add more, but discovery only needs the directory.
fn gitdir(ws: &Path, rel: &str) {
    std::fs::create_dir_all(ws.join(rel).join("hooks")).unwrap();
    std::fs::write(ws.join(rel).join("HEAD"), "ref: refs/heads/main\n").unwrap();
}

fn set(ws: &Path, rels: &[&str]) -> BTreeSet<PathBuf> {
    rels.iter().map(|rel| ws.join(rel)).collect()
}

#[test]
fn a_workspace_without_git_has_nothing_to_protect() {
    let (_d, ws) = workspace();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let index = discover(&ws, None);
    assert!(index.dot_gits.is_empty() && index.gitdirs.is_empty() && index.links.is_empty());
}

#[test]
fn worktrees_and_submodules_are_gitdirs_at_any_depth() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    std::fs::create_dir_all(ws.join(".git/worktrees/wt1")).unwrap();
    // A submodule named `a/b`, and a submodule `c` with its own submodule `d`.
    gitdir(&ws, ".git/modules/a/b");
    gitdir(&ws, ".git/modules/c");
    gitdir(&ws, ".git/modules/c/modules/d");
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(
        index.gitdirs,
        set(
            &ws,
            &[
                ".git",
                ".git/modules/a/b",
                ".git/modules/c",
                ".git/modules/c/modules/d",
                ".git/worktrees/wt1"
            ]
        )
    );
}

#[test]
fn nested_repositories_are_found_but_not_in_ignored_directories() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    std::fs::write(ws.join(".gitignore"), "ignored/\n").unwrap();
    gitdir(&ws, "vendor/lib/.git");
    gitdir(&ws, "ignored/repo/.git");
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "vendor/lib/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git", "vendor/lib/.git"]));
}

#[test]
fn a_submodule_gitfile_leads_to_its_gitdir() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, ".git/modules/sub");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    std::fs::write(ws.join("sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "sub/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git", ".git/modules/sub"]));
    assert!(
        index.links.contains(&ws.join(".git/modules/sub")),
        "{index:?}"
    );
}

#[test]
fn a_linked_worktree_leads_to_its_gitdir_and_the_common_one() {
    let (_d, ws) = workspace();
    gitdir(&ws, "main/.git");
    gitdir(&ws, "main/.git/worktrees/wt");
    std::fs::write(ws.join("main/.git/worktrees/wt/commondir"), "../..\n").unwrap();
    std::fs::create_dir_all(ws.join("wt")).unwrap();
    std::fs::write(ws.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &["main/.git", "wt/.git"]));
    assert_eq!(
        index.gitdirs,
        set(&ws, &["main/.git", "main/.git/worktrees/wt"])
    );
}

#[test]
fn symlinks_are_not_followed() {
    let (_d, ws) = workspace();
    let (_o, outside) = workspace();
    gitdir(&ws, ".git");
    gitdir(&outside, "repo/.git");
    std::fs::create_dir_all(outside.join("wts/x")).unwrap();
    symlink(outside.join("repo"), ws.join("linked")).unwrap();
    symlink(outside.join("wts"), ws.join(".git/worktrees")).unwrap();
    let index = discover(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git"]));
}

#[test]
fn the_skipped_directory_is_not_walked() {
    let (_d, ws) = workspace();
    gitdir(&ws, "quarantine/sub/.git");
    gitdir(&ws, "kept/.git");
    let index = discover(&ws, Some(&ws.join("quarantine")));
    assert_eq!(index.dot_gits, set(&ws, &["kept/.git"]));
}
