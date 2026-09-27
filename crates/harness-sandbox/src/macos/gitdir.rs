//! Finds the gitdir a workspace's `.git` leads to when `.git` is a symlink or
//! a gitfile, so the Seatbelt profile can protect it like `.git` itself.
//!
//! Git follows a `.git` symlink, and reads a `.git` file (a gitfile, also
//! when reached through a symlink) as `gitdir: <path>`, relative to the
//! directory that holds `.git` (setup.c `read_gitfile_gently`). In that
//! gitdir it reads `commondir`, relative to the gitdir, and takes config and
//! hooks from the dir it names (`get_common_dir_noenv`). Seatbelt matches
//! resolved paths, so none of these gitdirs has a `.git` component the
//! profile's `.git` rules would see.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// Symlinks followed on one path before giving up, as the kernel does for a
/// loop.
const MAX_SYMLINKS: usize = 40;

/// The most of a gitfile or `commondir` file read. A path is shorter.
const MAX_POINTER_BYTES: u64 = 4096;

/// What the profile protects for a workspace whose `.git` is a symlink or a
/// gitfile. Only paths inside the workspace are listed; nothing outside it is
/// writable from the workspace-write sandbox except the temp and cache roots.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LinkedGitdirs {
    /// Gitdirs git uses in this workspace: the one `.git` leads to, and the
    /// one its `commondir` file names.
    pub gitdirs: Vec<PathBuf>,
    /// Entries below the workspace that the way to those gitdirs passes
    /// through: each symlink and directory, the gitfile, and the gitdirs
    /// themselves. `.git` itself is left out: the profile protects every
    /// `.git` entry already.
    pub entries: Vec<PathBuf>,
}

/// The gitdirs `<workspace>/.git` leads to when it is a symlink or a gitfile.
/// `workspace` must be canonical. A plain `.git` directory needs nothing
/// extra: the profile's `.git` rules cover it.
///
/// A path that does not exist yet still counts (a dangling `.git` symlink
/// names where git would look once it is created). The entries on the way
/// are listed even when no gitdir is found, so a symlinked gitfile git
/// rejects today cannot be rewritten into one it accepts.
pub(crate) fn linked_gitdirs(workspace: &Path) -> LinkedGitdirs {
    let dot_git = workspace.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
        return LinkedGitdirs::default();
    };
    if !(meta.file_type().is_symlink() || meta.is_file()) {
        return LinkedGitdirs::default();
    }

    let mut visited = Vec::new();
    let mut gitdirs = Vec::new();
    if let Some(gitdir) = gitdir_of(workspace, &dot_git, &mut visited) {
        let common = pointer(&gitdir.join("commondir"), b"")
            .and_then(|common| follow(&gitdir.join(common), &mut visited));
        gitdirs.push(gitdir);
        gitdirs.extend(common);
    }

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in visited {
        if entry != workspace
            && entry != dot_git
            && within(&entry, workspace)
            && !entries.contains(&entry)
        {
            entries.push(entry);
        }
    }
    gitdirs.retain(|gitdir| within(gitdir, workspace));
    LinkedGitdirs { gitdirs, entries }
}

/// Where git finds the gitdir through `dot_git`: the directory it resolves
/// to, or, when it resolves to a regular file, the path that gitfile names.
fn gitdir_of(workspace: &Path, dot_git: &Path, visited: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let target = follow(dot_git, visited)?;
    if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
        // A directory, a path that does not exist, or something git cannot
        // read a gitfile from (a FIFO is never opened).
        return Some(target);
    }
    let named = pointer(&target, b"gitdir: ")?;
    // Relative to the directory holding `.git`, even through a symlink.
    follow(&workspace.join(named), visited)
}

/// The path a gitfile (`prefix` `gitdir: `) or a `commondir` file (no prefix)
/// holds, as git reads it: up to the first NUL, without trailing newlines.
/// `None` if it is not a regular file, lacks the prefix, or names nothing.
fn pointer(file: &Path, prefix: &[u8]) -> Option<PathBuf> {
    if !std::fs::metadata(file).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(MAX_POINTER_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = bytes.split(|&b| b == 0).next().unwrap_or_default();
    let mut path = text.strip_prefix(prefix)?;
    while let [rest @ .., b'\n' | b'\r'] = path {
        path = rest;
    }
    (!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path)))
}

/// Resolves the absolute `path` as the kernel would, following every symlink
/// on the way, and returns where it ends up. A component that does not exist
/// is taken as written, and so is everything after it. Every entry looked at
/// is pushed to `visited`, each symlink before it is followed. `None` after
/// [`MAX_SYMLINKS`] symlinks.
fn follow(path: &Path, visited: &mut Vec<PathBuf>) -> Option<PathBuf> {
    let mut resolved = PathBuf::from("/");
    let mut pending = VecDeque::new();
    prepend(&mut pending, path);
    let mut symlinks = 0;
    while let Some(name) = pending.pop_front() {
        let Some(name) = name else {
            resolved.pop();
            continue;
        };
        let next = resolved.join(&name);
        visited.push(next.clone());
        // `readlink` fails for anything but a symlink.
        match std::fs::read_link(&next) {
            Ok(target) => {
                symlinks += 1;
                if symlinks > MAX_SYMLINKS {
                    return None;
                }
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                prepend(&mut pending, &target);
            }
            _ => resolved = next,
        }
    }
    Some(resolved)
}

/// Puts `path`'s components at the front of `pending`: a name, or `None` for
/// `..`. The root and `.` add nothing.
fn prepend(pending: &mut VecDeque<Option<OsString>>, path: &Path) {
    for component in path.components().rev() {
        match component {
            Component::Normal(name) => pending.push_front(Some(name.to_owned())),
            Component::ParentDir => pending.push_front(None),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
}

/// Whether `path` is `base` or below it, comparing names ASCII
/// case-insensitively: on the default case-insensitive volume a symlink may
/// spell the workspace in another case, and Seatbelt's rules match either.
fn within(path: &Path, base: &Path) -> bool {
    let mut components = path.components();
    base.components().all(|b| {
        components.next().is_some_and(|c| {
            c.as_os_str()
                .as_bytes()
                .eq_ignore_ascii_case(b.as_os_str().as_bytes())
        })
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    fn mkdirs(ws: &Path, rel: &str) {
        std::fs::create_dir_all(ws.join(rel)).unwrap();
    }

    /// Asserts the gitdirs found, and that `entries` are among the protected
    /// entries.
    #[track_caller]
    fn check(ws: &Path, gitdirs: &[&str], entries: &[&str]) {
        let found = linked_gitdirs(ws);
        let want: Vec<PathBuf> = gitdirs.iter().map(|g| ws.join(g)).collect();
        assert_eq!(found.gitdirs, want, "{found:?}");
        for entry in entries {
            assert!(
                found.entries.contains(&ws.join(entry)),
                "{entry} is not protected: {found:?}"
            );
        }
        for entry in &found.entries {
            assert!(
                entry.starts_with(ws) && entry != ws && entry != &ws.join(".git"),
                "{entry:?} is not below the workspace, or is `.git`"
            );
        }
    }

    #[test]
    fn a_missing_or_plain_git_dir_needs_nothing_extra() {
        let (_d, ws) = workspace();
        assert_eq!(linked_gitdirs(&ws), LinkedGitdirs::default());
        mkdirs(&ws, ".git/hooks");
        assert_eq!(linked_gitdirs(&ws), LinkedGitdirs::default());
    }

    #[test]
    fn a_symlinked_git_leads_to_its_target() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "gitstuff");
        symlink("gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["gitstuff"], &["gitstuff"]);
    }

    #[test]
    fn every_link_and_directory_on_the_way_is_protected() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "real/gitstuff");
        symlink("real", ws.join("link")).unwrap();
        symlink("link/gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["real/gitstuff"], &["link", "real", "real/gitstuff"]);
    }

    #[test]
    fn a_dotdot_after_a_symlink_leaves_its_target() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "a/b");
        mkdirs(&ws, "a/gitstuff");
        symlink("a/b", ws.join("ab")).unwrap();
        symlink("ab/../gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["a/gitstuff"], &["ab", "a", "a/b", "a/gitstuff"]);
    }

    #[test]
    fn a_gitfile_names_its_gitdir_relative_to_the_workspace() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "meta/repo");
        std::fs::write(ws.join(".git"), "gitdir: meta/repo\r\n").unwrap();
        check(&ws, &["meta/repo"], &["meta", "meta/repo"]);

        let absolute = format!("gitdir: {}\n", ws.join("meta/repo").display());
        std::fs::write(ws.join(".git"), absolute).unwrap();
        check(&ws, &["meta/repo"], &["meta", "meta/repo"]);
    }

    #[test]
    fn a_symlinked_gitfile_is_read_relative_to_the_workspace() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "meta");
        mkdirs(&ws, "repo");
        std::fs::write(ws.join("meta/gitfile"), "gitdir: repo\n").unwrap();
        symlink("meta/gitfile", ws.join(".git")).unwrap();
        check(&ws, &["repo"], &["meta", "meta/gitfile", "repo"]);
    }

    #[test]
    fn a_missing_target_still_counts() {
        let (_d, ws) = workspace();
        symlink("missing/gitstuff", ws.join(".git")).unwrap();
        check(&ws, &["missing/gitstuff"], &["missing", "missing/gitstuff"]);

        std::fs::remove_file(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: gone/repo\n").unwrap();
        check(&ws, &["gone/repo"], &["gone", "gone/repo"]);
    }

    #[test]
    fn the_gitdirs_commondir_is_followed() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "meta/main");
        mkdirs(&ws, "meta/wt");
        std::fs::write(ws.join("meta/wt/commondir"), "../main\n").unwrap();
        std::fs::write(ws.join(".git"), "gitdir: meta/wt\n").unwrap();
        check(
            &ws,
            &["meta/wt", "meta/main"],
            &["meta", "meta/wt", "meta/main"],
        );
    }

    #[test]
    fn a_gitdir_outside_the_workspace_is_left_out() {
        let (_d, ws) = workspace();
        let (_o, outside) = workspace();
        mkdirs(&outside, "gitstuff");
        symlink(outside.join("gitstuff"), ws.join(".git")).unwrap();
        check(&ws, &[], &[]);

        std::fs::remove_file(ws.join(".git")).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: ../elsewhere\n").unwrap();
        check(&ws, &[], &[]);
    }

    #[test]
    fn a_gitdir_spelled_in_another_case_is_still_inside() {
        let (_d, ws) = workspace();
        mkdirs(&ws, "gitstuff");
        let upper = PathBuf::from(ws.to_str().unwrap().to_uppercase());
        if !upper.join("gitstuff").is_dir() {
            eprintln!("case-sensitive volume: skipping");
            return;
        }
        symlink(upper.join("gitstuff"), ws.join(".git")).unwrap();
        let found = linked_gitdirs(&ws);
        assert_eq!(found.gitdirs, vec![upper.join("gitstuff")], "{found:?}");
    }

    #[test]
    fn a_symlink_loop_or_an_invalid_gitfile_leads_nowhere() {
        let (_d, ws) = workspace();
        symlink(".git", ws.join(".git")).unwrap();
        check(&ws, &[], &[]);
        std::fs::remove_file(ws.join(".git")).unwrap();

        // Git wants exactly `gitdir: ` and a path.
        for text in ["gitdir:repo\n", "gitdir: \n", "nonsense\n", ""] {
            std::fs::write(ws.join(".git"), text).unwrap();
            check(&ws, &[], &[]);
        }

        // The gitfile behind a symlink stays protected even when it is invalid.
        std::fs::remove_file(ws.join(".git")).unwrap();
        mkdirs(&ws, "meta");
        std::fs::write(ws.join("meta/gitfile"), "nonsense\n").unwrap();
        symlink("meta/gitfile", ws.join(".git")).unwrap();
        check(&ws, &[], &["meta", "meta/gitfile"]);
    }

    #[test]
    fn a_fifo_is_never_opened() {
        let (_d, ws) = workspace();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(ws.join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        symlink("fifo", ws.join(".git")).unwrap();
        // Reading it would block forever; it is taken as the gitdir's path.
        check(&ws, &["fifo"], &["fifo"]);
    }
}
