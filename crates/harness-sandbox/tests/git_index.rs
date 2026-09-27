//! `gitmeta::discover` on real directory trees. Platform-neutral: runs on macOS and Linux.
//!
//! The workspace is hostile: a sandboxed command can write any of it. Every
//! `discover` here runs on a helper thread that must finish within [`LIMIT`],
//! so a probe that would block the harness (a FIFO, `/dev/zero`) fails its
//! test instead of hanging the run. Every probe stays inside a temp dir.

use std::collections::BTreeSet;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use harness_sandbox::gitmeta::{GitIndex, discover};

/// How long one `discover` of these small trees may take.
const LIMIT: Duration = Duration::from_secs(10);

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

fn write(ws: &Path, rel: &str, text: &str) {
    let path = ws.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn mkfifo(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = std::process::Command::new("/usr/bin/mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

fn set(ws: &Path, rels: &[&str]) -> BTreeSet<PathBuf> {
    rels.iter().map(|rel| ws.join(rel)).collect()
}

/// `discover`, which must finish within [`LIMIT`]. A thread that does not
/// is left behind, blocked; the test fails either way.
fn index(ws: &Path, skip: Option<&Path>) -> GitIndex {
    let (ws, skip) = (ws.to_path_buf(), skip.map(Path::to_path_buf));
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _ = tx.send(discover(&ws, skip.as_deref()));
    });
    match rx.recv_timeout(LIMIT) {
        Ok(index) => index,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("discover took longer than {LIMIT:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => match thread.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("discover returned nothing"),
        },
    }
}

/// [`index`], asserting that nothing kept the walk from seeing everything.
#[track_caller]
fn complete(ws: &Path, skip: Option<&Path>) -> GitIndex {
    let index = index(ws, skip);
    assert!(!index.incomplete, "{index:?}");
    index
}

#[test]
fn a_workspace_without_git_has_nothing_to_protect() {
    let (_d, ws) = workspace();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    let index = complete(&ws, None);
    assert!(index.dot_gits.is_empty() && index.gitdirs.is_empty() && index.links.is_empty());
}

#[test]
fn worktrees_and_submodules_are_gitdirs() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    std::fs::create_dir_all(ws.join(".git/worktrees/wt1")).unwrap();
    // A submodule named `a/b`, and a submodule `c` with its own submodule `d`.
    gitdir(&ws, ".git/modules/a/b");
    gitdir(&ws, ".git/modules/c");
    gitdir(&ws, ".git/modules/c/modules/d");
    let index = complete(&ws, None);
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
fn a_submodule_gitdir_many_levels_down_is_found() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    let nine = ".git/modules/1/2/3/4/5/6/7/8/9";
    // With its `hooks`, 64 levels below `modules/`: as deep as the walk reads.
    let deep = format!(".git/modules/{}", vec!["d"; 63].join("/"));
    gitdir(&ws, nine);
    gitdir(&ws, &deep);
    let index = complete(&ws, None);
    assert_eq!(index.gitdirs, set(&ws, &[".git", nine, &deep]));
}

#[test]
fn a_directory_below_the_depth_cap_makes_the_index_incomplete() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, &format!(".git/modules/{}", vec!["d"; 65].join("/")));
    let index = index(&ws, None);
    assert_eq!(index.gitdirs, set(&ws, &[".git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn a_junk_head_does_not_hide_the_submodules_below_it() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(&ws, ".git/modules/a/HEAD", "junk\n");
    gitdir(&ws, ".git/modules/a/b");
    let index = complete(&ws, None);
    assert_eq!(
        index.gitdirs,
        set(&ws, &[".git", ".git/modules/a", ".git/modules/a/b"])
    );
}

#[test]
fn the_object_store_of_a_submodule_gitdir_is_not_searched() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, ".git/modules/c");
    for skipped in ["objects", "refs", "logs", "lfs", "info"] {
        gitdir(&ws, &format!(".git/modules/c/{skipped}/x"));
    }
    let index = complete(&ws, None);
    assert_eq!(index.gitdirs, set(&ws, &[".git", ".git/modules/c"]));
}

#[test]
fn nested_repositories_are_found_but_not_in_ignored_directories() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    std::fs::write(ws.join(".gitignore"), "ignored/\n").unwrap();
    gitdir(&ws, "vendor/lib/.git");
    gitdir(&ws, "ignored/repo/.git");
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "vendor/lib/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git", "vendor/lib/.git"]));
}

#[test]
fn ignore_rules_chain_per_directory_with_negations() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(
        &ws,
        ".gitignore",
        "# comment\nvendor/\nbuild*/\n!build-keep/\n",
    );
    write(&ws, "sub/.gitignore", "!vendor/\n");
    for repo in [
        "vendor/r",
        "build-x/r",
        "build-keep/r",
        "sub/vendor/r",
        "sub/build-y/r",
    ] {
        gitdir(&ws, &format!("{repo}/.git"));
    }
    let index = complete(&ws, None);
    assert_eq!(
        index.dot_gits,
        set(&ws, &[".git", "build-keep/r/.git", "sub/vendor/r/.git"])
    );
}

#[test]
fn info_exclude_rules_apply() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(&ws, ".git/info/exclude", "excluded/\n");
    gitdir(&ws, "excluded/r/.git");
    gitdir(&ws, "kept/r/.git");
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "kept/r/.git"]));
}

#[test]
fn a_nested_repository_has_its_own_rules() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(&ws, ".gitignore", "build/\n");
    gitdir(&ws, "build/r/.git");
    // Git reads `lib` as a repository of its own: the outer rules stop there.
    gitdir(&ws, "lib/.git");
    gitdir(&ws, "lib/build/r/.git");
    write(&ws, "lib/.git/info/exclude", "tmp/\n");
    gitdir(&ws, "lib/tmp/r/.git");
    let index = complete(&ws, None);
    assert_eq!(
        index.dot_gits,
        set(&ws, &[".git", "lib/.git", "lib/build/r/.git"])
    );
}

#[test]
fn ignore_rules_from_above_the_workspace_do_not_apply() {
    let (_d, base) = workspace();
    gitdir(&base, ".git");
    write(&base, ".gitignore", "*\n");
    let ws = base.join("ws");
    gitdir(&ws, "lib/.git");
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &["lib/.git"]));
}

#[test]
fn a_submodule_gitfile_leads_to_its_gitdir() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    // The submodule's gitdir sits behind an ignored directory, where neither
    // the walk nor `modules/**` finds it: only the gitfile leads there.
    write(&ws, ".gitignore", "/store/\n");
    gitdir(&ws, "store/modules/sub");
    write(&ws, "sub/.git", "gitdir: ../store/modules/sub\n");
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "sub/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git", "store/modules/sub"]));
    assert_eq!(
        index.links,
        set(&ws, &["store", "store/modules", "store/modules/sub"])
    );
}

#[test]
fn a_linked_worktree_leads_to_its_gitdir_and_the_common_one() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    // The main repository sits behind an ignored directory: only the
    // worktree's `commondir` leads to its gitdir.
    write(&ws, ".gitignore", "/hidden/\n");
    gitdir(&ws, "hidden/main/.git");
    gitdir(&ws, "hidden/main/.git/worktrees/wt");
    write(&ws, "hidden/main/.git/worktrees/wt/commondir", "../..\n");
    write(&ws, "wt/.git", "gitdir: ../hidden/main/.git/worktrees/wt\n");
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "wt/.git"]));
    assert_eq!(
        index.gitdirs,
        set(
            &ws,
            &[".git", "hidden/main/.git", "hidden/main/.git/worktrees/wt"]
        )
    );
}

#[test]
fn every_gitdirs_commondir_is_followed_and_its_links_recorded() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(&ws, ".gitignore", "/hidden/\n");
    gitdir(&ws, "hidden/main/.git");
    // A worktree gitdir found through `worktrees/`, not a gitfile, whose
    // `commondir` goes through a symlink to a gitdir the walk skips.
    gitdir(&ws, ".git/worktrees/wt");
    write(&ws, ".git/worktrees/wt/commondir", "../../../link/.git\n");
    symlink("hidden/main", ws.join("link")).unwrap();
    let index = complete(&ws, None);
    assert_eq!(
        index.gitdirs,
        set(&ws, &[".git", ".git/worktrees/wt", "hidden/main/.git"])
    );
    assert_eq!(
        index.links,
        set(&ws, &["link", "hidden", "hidden/main", "hidden/main/.git"])
    );
}

#[test]
fn a_top_level_symlinked_dot_git_leads_to_its_target() {
    let (_d, ws) = workspace();
    gitdir(&ws, "meta/repo");
    symlink("meta/repo", ws.join(".git")).unwrap();
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(index.gitdirs, set(&ws, &["meta/repo"]));
    assert_eq!(index.links, set(&ws, &["meta", "meta/repo"]));
}

#[test]
fn a_top_level_gitfile_leads_to_its_gitdir() {
    let (_d, ws) = workspace();
    gitdir(&ws, "meta/repo");
    gitdir(&ws, "meta/repo/worktrees/wt");
    write(&ws, ".git", "gitdir: meta/repo\n");
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(
        index.gitdirs,
        set(&ws, &["meta/repo", "meta/repo/worktrees/wt"])
    );
    assert_eq!(index.links, set(&ws, &["meta", "meta/repo"]));
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
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert_eq!(index.gitdirs, set(&ws, &[".git"]));
}

#[test]
fn the_skipped_directory_is_not_walked() {
    let (_d, ws) = workspace();
    gitdir(&ws, "quarantine/sub/.git");
    gitdir(&ws, "kept/.git");
    let index = complete(&ws, Some(&ws.join("quarantine")));
    assert_eq!(index.dot_gits, set(&ws, &["kept/.git"]));
}

#[test]
fn an_unreadable_directory_makes_the_index_incomplete() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, "locked/r/.git");
    let locked = ws.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok();
    let index = index(&ws, None);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("running as root: skipping");
        return;
    }
    assert_eq!(index.dot_gits, set(&ws, &[".git"]));
    assert!(index.incomplete, "{index:?}");
}

// A sandboxed command can plant any of the following; `discover` runs in the
// harness around every command, so none may block it or exhaust its memory.

#[test]
fn a_fifo_gitignore_is_never_opened() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, "sub/r/.git");
    mkfifo(&ws.join("sub/.gitignore"));
    let index = index(&ws, None);
    // Its rules are unknown: none apply, and the index says so.
    assert_eq!(index.dot_gits, set(&ws, &[".git", "sub/r/.git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn a_fifo_info_exclude_is_never_opened() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    mkfifo(&ws.join(".git/info/exclude"));
    gitdir(&ws, "r/.git");
    let index = index(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "r/.git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn a_fifo_commondir_is_never_opened() {
    let (_d, ws) = workspace();
    gitdir(&ws, "meta/wt");
    mkfifo(&ws.join("meta/wt/commondir"));
    let gitfile = format!("gitdir: {}\n", ws.join("meta/wt").display());
    write(&ws, "wt/.git", &gitfile);
    let index = complete(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &["wt/.git"]));
    assert_eq!(index.gitdirs, set(&ws, &["meta/wt"]));
}

#[test]
fn a_gitignore_symlinked_to_dev_zero_is_never_read() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, "sub/r/.git");
    symlink("/dev/zero", ws.join("sub/.gitignore")).unwrap();
    let index = index(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "sub/r/.git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn a_gitignore_symlinked_outside_the_workspace_is_not_followed() {
    let (_d, ws) = workspace();
    let (_o, outside) = workspace();
    write(&outside, "rules", "*\n");
    gitdir(&ws, ".git");
    gitdir(&ws, "r/.git");
    symlink(outside.join("rules"), ws.join(".gitignore")).unwrap();
    let index = index(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "r/.git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn an_oversize_gitignore_is_not_applied() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    gitdir(&ws, "r/.git");
    // Over 1 MiB: none of it applies, not even the part before the limit.
    let mut rules = String::from("r/\n");
    while rules.len() <= 1 << 20 {
        rules.push_str("# padding padding padding padding padding padding\n");
    }
    write(&ws, ".gitignore", &rules);
    let index = index(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "r/.git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn below_an_unusable_gitignore_no_rules_apply() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(&ws, ".gitignore", "vendor/\n");
    gitdir(&ws, "vendor/r/.git");
    // What `sub/.gitignore` says is unknown: it might re-include `vendor/`.
    mkfifo(&ws.join("sub/.gitignore"));
    gitdir(&ws, "sub/vendor/r/.git");
    let index = index(&ws, None);
    assert_eq!(index.dot_gits, set(&ws, &[".git", "sub/vendor/r/.git"]));
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn ignore_files_past_4_mib_in_all_are_not_applied() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    let mut rules = String::from("r/\n");
    while rules.len() < 900 << 10 {
        rules.push_str("# padding padding padding padding padding padding\n");
    }
    for i in 0..5 {
        write(&ws, &format!("a{i}/.gitignore"), &rules);
        gitdir(&ws, &format!("a{i}/r/.git"));
    }
    let index = index(&ws, None);
    // Four fit; below the fifth, whichever the walk reaches last, none apply.
    assert_eq!(index.dot_gits.len(), 2, "{index:?}");
    assert!(index.incomplete, "{index:?}");
}

#[test]
fn below_32_nested_gitignores_no_rules_apply() {
    let (_d, ws) = workspace();
    gitdir(&ws, ".git");
    write(&ws, ".gitignore", "ignored/\n");
    let mut dir = String::new();
    for level in 1..=32 {
        dir.push_str(&format!("l{level}/"));
        write(&ws, &format!("{dir}.gitignore"), "# nothing\n");
        gitdir(&ws, &format!("{dir}ignored/r/.git"));
    }
    let index = index(&ws, None);
    // The 32nd nested file is the 33rd in the chain: from there, none apply.
    assert_eq!(
        index.dot_gits,
        set(&ws, &[".git", &format!("{dir}ignored/r/.git")])
    );
    assert!(index.incomplete, "{index:?}");
}
