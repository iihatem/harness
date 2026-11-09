//! `.worktreeinclude`: gitignored files a worktree needs (an `.env`, installed dependencies)
//! and that a fresh checkout does not have. The file, at the repository's root, lists what to
//! copy in gitignore syntax; only files git itself ignores are copied (never tracked ones,
//! never anything merely untracked), nothing is overwritten, and a symbolic link is neither
//! followed nor copied, in the main checkout or the worktree.

use std::path::{Component, Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::git::Git;

/// The most of the list file that is read.
const LIST_CAP: u64 = 64 * 1024;

/// How much one copy may be.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub files: usize,
    pub bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            files: 20_000,
            bytes: 1 << 30,
        }
    }
}

/// What a copy did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Included {
    pub copied: usize,
    /// Things worth telling the user: a limit that was reached, an unreadable file.
    pub notes: Vec<String>,
    /// What was copied, as paths below the repository's root. Harness never commits them to the
    /// agent's branch: the rule that ignores a file in the main checkout may be one that is not
    /// committed, and then the new worktree does not ignore it.
    pub paths: Vec<String>,
}

struct Copier<'a> {
    list: Gitignore,
    root: &'a Path,
    dest: &'a Path,
    limits: Limits,
    bytes: u64,
    done: Included,
    full: bool,
}

/// [`copy_with`] with the default limits.
pub fn copy_included(git: &Git, root: &Path, dest: &Path) -> Included {
    copy_with(git, root, dest, Limits::default())
}

/// Copies what `<root>/.worktreeinclude` lists, of what git ignores in the repository at
/// `root`, into the worktree `dest`.
pub fn copy_with(git: &Git, root: &Path, dest: &Path, limits: Limits) -> Included {
    let list_path = root.join(".worktreeinclude");
    let regular = std::fs::symlink_metadata(&list_path).is_ok_and(|m| m.is_file());
    let Some(text) = regular
        .then(|| std::fs::read(&list_path).ok())
        .flatten()
        .map(|bytes| {
            String::from_utf8_lossy(&bytes[..bytes.len().min(LIST_CAP as usize)]).into_owned()
        })
    else {
        return Included::default();
    };
    let mut builder = GitignoreBuilder::new(root);
    for line in text.lines() {
        let _ = builder.add_line(None, line);
    }
    let Ok(list) = builder.build() else {
        return Included::default();
    };
    let ignored = match git.run(&[
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ]) {
        Ok(out) => out,
        Err(e) => {
            return Included {
                copied: 0,
                notes: vec![format!("could not list the ignored files: {e}")],
                paths: Vec::new(),
            };
        }
    };
    let mut copier = Copier {
        list,
        root,
        dest,
        limits,
        bytes: 0,
        done: Included::default(),
        full: false,
    };
    for entry in ignored.split('\0').filter(|e| !e.is_empty()) {
        if copier.full {
            break;
        }
        let rel = PathBuf::from(entry.trim_end_matches('/'));
        copier.visit(&rel);
    }
    copier.done
}

impl Copier<'_> {
    fn listed(&self, rel: &Path, dir: bool) -> bool {
        self.list.matched_path_or_any_parents(rel, dir).is_ignore()
    }

    /// `rel` (a path below the root): a file listed is copied; a directory listed (or below a
    /// listed one) is copied whole; any other directory is looked into.
    fn visit(&mut self, rel: &Path) {
        if self.full {
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(self.root.join(rel)) else {
            return;
        };
        // A link is never followed, and never copied.
        if meta.file_type().is_symlink() {
            return;
        }
        if meta.is_file() {
            if self.listed(rel, false) {
                self.copy_file(rel, meta.len());
            }
        } else if meta.is_dir() {
            let whole = self.listed(rel, true);
            let Ok(entries) = std::fs::read_dir(self.root.join(rel)) else {
                return;
            };
            let mut children: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .collect();
            children.sort();
            for child in children {
                let child = rel.join(child);
                if whole {
                    self.visit_whole(&child);
                } else {
                    self.visit(&child);
                }
            }
        }
    }

    /// Inside a directory git ignores as a whole, and the list names, everything is copied, but
    /// for links. (What git does not ignore is never reached: a directory with such files in
    /// it is not listed as one by git, only the files it does ignore are.)
    fn visit_whole(&mut self, rel: &Path) {
        if self.full {
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(self.root.join(rel)) else {
            return;
        };
        if meta.file_type().is_symlink() {
            return;
        }
        if meta.is_file() {
            if self
                .list
                .matched_path_or_any_parents(rel, false)
                .is_ignore()
            {
                self.copy_file(rel, meta.len());
            }
        } else if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(self.root.join(rel))
        {
            let mut children: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .collect();
            children.sort();
            for child in children {
                self.visit_whole(&rel.join(child));
            }
        }
    }

    fn copy_file(&mut self, rel: &Path, len: u64) {
        if self.done.copied >= self.limits.files
            || self.bytes.saturating_add(len) > self.limits.bytes
        {
            self.full = true;
            self.done.notes.push(format!(
                "the copy for .worktreeinclude stopped after {} files ({} bytes): it reached its limit",
                self.done.copied, self.bytes
            ));
            return;
        }
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return;
        }
        // No component of the destination may be a link: writing below one would leave the
        // worktree.
        let mut at = self.dest.to_path_buf();
        let parent = rel.parent().unwrap_or(Path::new(""));
        for component in parent.components() {
            at.push(component);
            match std::fs::symlink_metadata(&at) {
                Ok(m) if m.file_type().is_symlink() => return,
                Ok(_) => {}
                Err(_) => {
                    if std::fs::create_dir(&at).is_err() {
                        return;
                    }
                }
            }
        }
        let target = self.dest.join(rel);
        if std::fs::symlink_metadata(&target).is_ok() {
            return;
        }
        match std::fs::copy(self.root.join(rel), &target) {
            Ok(_) => {
                self.done.copied += 1;
                self.bytes += len;
                self.done.paths.push(rel.to_string_lossy().into_owned());
            }
            Err(e) => self
                .done
                .notes
                .push(format!("could not copy {}: {e}", rel.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::{
        Git,
        testing::{git as plain, init_repo, write},
    };

    /// A repository that ignores what a typical project does, with some ignored files in it.
    fn project(include: &str) -> (tempfile::TempDir, tempfile::TempDir) {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        write(
            repo.path(),
            ".gitignore",
            ".env\n*.local\nnode_modules/\nsecret.key\nbuild/\n",
        );
        plain(repo.path(), &["add", "-A"]);
        plain(repo.path(), &["commit", "-q", "-m", "ignore"]);
        write(repo.path(), ".env", "TOKEN=1\n");
        write(repo.path(), "config/dev.local", "local\n");
        write(repo.path(), "node_modules/pkg/index.js", "module\n");
        write(repo.path(), "secret.key", "key\n");
        write(repo.path(), "build/out.o", "object\n");
        write(repo.path(), "notes.txt", "untracked, not ignored\n");
        write(repo.path(), ".worktreeinclude", include);
        let dest = tempfile::tempdir().unwrap();
        plain(
            repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                dest.path().join("wt").to_str().unwrap(),
            ],
        );
        (repo, dest)
    }

    fn copy(repo: &tempfile::TempDir, dest: &tempfile::TempDir) -> Included {
        let git = Git::open(repo.path()).unwrap();
        copy_included(&git, repo.path(), &dest.path().join("wt"))
    }

    #[test]
    fn ignored_files_that_the_list_names_are_copied_and_nothing_else() {
        let (repo, dest) =
            project("# what the build needs\n.env\nnode_modules/\n*.local\nnotes.txt\nREADME.md\n");
        let done = copy(&repo, &dest);
        let wt = dest.path().join("wt");
        assert!(wt.join(".env").exists());
        assert!(wt.join("node_modules/pkg/index.js").exists());
        assert!(wt.join("config/dev.local").exists());
        assert!(!wt.join("secret.key").exists(), "ignored but not listed");
        assert!(!wt.join("build/out.o").exists(), "ignored but not listed");
        assert!(!wt.join("notes.txt").exists(), "listed but not ignored");
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "hello\n"
        );
        assert_eq!(done.copied, 3);
    }

    #[test]
    fn gitignore_syntax_negation_and_anchors_apply() {
        let (repo, dest) = project("*.local\n!config/dev.local\n/secret.key\n");
        write(repo.path(), "other.local", "x\n");
        let done = copy(&repo, &dest);
        let wt = dest.path().join("wt");
        assert!(wt.join("other.local").exists());
        assert!(!wt.join("config/dev.local").exists(), "negated");
        assert!(wt.join("secret.key").exists(), "anchored at the root");
        assert_eq!(done.copied, 2);
    }

    #[test]
    fn in_a_listed_directory_only_what_git_ignores_is_copied() {
        let (repo, dest) = project("cache/\n");
        // `cache/*` ignores everything in it but `keep.txt`, which git does not ignore.
        write(repo.path(), ".gitignore", "cache/*\n!cache/keep.txt\n");
        write(repo.path(), "cache/ignored.bin", "ignored\n");
        write(repo.path(), "cache/keep.txt", "not ignored\n");
        write(repo.path(), "cache/deep/inner.bin", "ignored too\n");
        let done = copy(&repo, &dest);
        let wt = dest.path().join("wt");
        assert!(wt.join("cache/ignored.bin").exists());
        assert!(wt.join("cache/deep/inner.bin").exists());
        assert!(
            !wt.join("cache/keep.txt").exists(),
            "git does not ignore it: {done:?}"
        );
    }

    #[test]
    fn no_list_copies_nothing() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let dest = tempfile::tempdir().unwrap();
        let done = copy_included(&Git::open(repo.path()).unwrap(), repo.path(), dest.path());
        assert_eq!(done, Included::default());
    }

    #[test]
    fn a_symlink_is_never_followed_or_copied() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "private.txt", "outside\n");
        let (repo, dest) = project(".env\n*.local\nnode_modules/\nlink\nlinked/\n");
        // An ignored file that is a link out, a listed directory that is one, and a link
        // inside a listed directory.
        std::fs::remove_file(repo.path().join(".env")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("private.txt"), repo.path().join(".env"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join("node_modules/escape"))
            .unwrap();
        write(
            repo.path(),
            ".gitignore",
            ".env\n*.local\nnode_modules/\nsecret.key\nbuild/\nlink\nlinked/\n",
        );
        std::os::unix::fs::symlink(outside.path(), repo.path().join("linked")).unwrap();
        let done = copy(&repo, &dest);
        let wt = dest.path().join("wt");
        assert!(
            std::fs::symlink_metadata(wt.join(".env")).is_err(),
            "the link was not copied"
        );
        assert!(!wt.join("node_modules/escape").exists());
        assert!(!wt.join("linked").exists());
        assert!(wt.join("node_modules/pkg/index.js").exists());
        assert!(!done.notes.is_empty() || done.copied >= 2);
        assert!(!outside.path().join("node_modules").exists());
    }

    #[test]
    fn nothing_is_written_through_a_link_in_the_worktree() {
        let outside = tempfile::tempdir().unwrap();
        let (repo, dest) = project("node_modules/\n");
        // The worktree has a link where the main checkout has a directory.
        let wt = dest.path().join("wt");
        std::os::unix::fs::symlink(outside.path(), wt.join("node_modules")).unwrap();
        copy(&repo, &dest);
        assert!(
            std::fs::read_dir(outside.path()).unwrap().next().is_none(),
            "something was written outside"
        );
    }

    #[test]
    fn a_file_already_in_the_worktree_is_not_overwritten() {
        let (repo, dest) = project(".env\n");
        write(&dest.path().join("wt"), ".env", "TOKEN=other\n");
        copy(&repo, &dest);
        assert_eq!(
            std::fs::read_to_string(dest.path().join("wt/.env")).unwrap(),
            "TOKEN=other\n"
        );
    }

    #[test]
    fn a_list_that_is_a_link_is_not_read() {
        let (repo, dest) = project(".env\n");
        std::fs::remove_file(repo.path().join(".worktreeinclude")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        write(elsewhere.path(), "list", ".env\n");
        std::os::unix::fs::symlink(
            elsewhere.path().join("list"),
            repo.path().join(".worktreeinclude"),
        )
        .unwrap();
        let done = copy(&repo, &dest);
        assert_eq!(done.copied, 0);
        assert!(!dest.path().join("wt/.env").exists());
    }

    #[test]
    fn too_many_files_stop_the_copy_and_say_so() {
        let (repo, dest) = project("node_modules/\n");
        for n in 0..10 {
            write(repo.path(), &format!("node_modules/p/f{n}.js"), "x\n");
        }
        let git = Git::open(repo.path()).unwrap();
        let done = copy_with(
            &git,
            repo.path(),
            &dest.path().join("wt"),
            Limits {
                files: 4,
                bytes: u64::MAX,
            },
        );
        assert_eq!(done.copied, 4);
        assert!(
            done.notes.iter().any(|n| n.contains("stopped")),
            "{:?}",
            done.notes
        );
    }
}
