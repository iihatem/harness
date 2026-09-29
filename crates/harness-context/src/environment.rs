//! Facts about where the session runs, captured once when it starts.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};

use harness_core::subprocess::output_within;

use crate::{project::repo_root, read::read_regular};

/// How long each `git` query may take before its answer is left out.
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Commands run in `/bin/bash`, which on macOS is bash 3.2. It rejects the common
/// `git commit -m "$(cat <<'EOF' … EOF)"` idiom with a syntax error whenever the message has an
/// odd number of `'`, `"` or backticks, about one real commit message in ten.
const MACOS_COMMIT_NOTE: &str = "Shell: macOS's /bin/bash 3.2. For a multi-line commit message use `git commit -F - <<'EOF'` … `EOF`, not `git commit -m \"$(cat <<'EOF' … EOF)\"`, which bash 3.2 rejects when the message has an odd number of quotes or backticks.\n";

/// The environment as it was when the session started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub cwd: PathBuf,
    pub os: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// `None` outside a git repository.
    pub git: Option<GitState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitState {
    /// The branch name, or `detached HEAD at <commit>`; `None` if git could not say.
    pub head: Option<String>,
    /// Whether the work tree has uncommitted changes (untracked files count); `None` if git could
    /// not say in time.
    pub dirty: Option<bool>,
}

/// Captures the environment of a session starting now in `cwd` on `date`.
pub fn capture(cwd: &Path, date: &str) -> Environment {
    Environment {
        cwd: cwd.to_path_buf(),
        os: std::env::consts::OS.to_string(),
        date: date.to_string(),
        git: repo_root(cwd).map(|_| git_state(cwd)),
    }
}

/// Settings given on every git command line, where they outrank the repository's own
/// configuration: git runs no file-system monitor or hook that configuration names. (The
/// repository may come from anywhere, and this runs before any approval, outside the sandbox.)
const OVERRIDES: [&str; 2] = ["core.fsmonitor=false", "core.hooksPath=/dev/null"];

/// Runs git in `cwd` with [`OVERRIDES`]; `None` when it cannot run or does not finish in time.
fn run_git(cwd: &Path, args: &[&str]) -> Option<Output> {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd);
    for setting in OVERRIDES {
        command.args(["-c", setting]);
    }
    command.args(args);
    output_within(&mut command, GIT_TIMEOUT).ok()?
}

/// git's trimmed output, when it succeeds.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = run_git(cwd, args)?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    })
}

fn git_state(cwd: &Path) -> GitState {
    let head = repo_root(cwd)
        .and_then(|root| head_from_file(&root))
        .or_else(|| {
            git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
                .filter(|branch| !branch.is_empty())
                .or_else(|| {
                    git(cwd, &["rev-parse", "--short", "HEAD"])
                        .map(|commit| format!("detached HEAD at {commit}"))
                })
        });
    // `status` runs the clean filter of every file whose stat changed, so with a filter driver
    // the repository's own configuration defines, the answer is left out. Reading the
    // configuration runs nothing. Submodules are left alone: `status` would run `git status` in
    // each, with the submodule's own configuration.
    let dirty = if repository_defines_filters(cwd) {
        None
    } else {
        git(
            cwd,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--ignore-submodules=all",
            ],
        )
        .map(|status| !status.is_empty())
    };
    GitState { head, dirty }
}

/// Whether the repository's own configuration for `cwd` (its `local` and `worktree` scopes, with
/// the files they include, which git reports under the including file's scope) defines any
/// filter driver (`filter.<driver>.clean`, `smudge` or `process`); also when git cannot say, as a
/// git older than 2.26 cannot (no `--show-scope`). A driver in the user's global or system
/// configuration, such as git-lfs's, is the user's own program, which their own `git status`
/// runs as well.
fn repository_defines_filters(cwd: &Path) -> bool {
    let Some(output) = run_git(
        cwd,
        &[
            "config",
            "--show-scope",
            "--includes",
            "--get-regexp",
            r"^filter\.",
        ],
    ) else {
        return true;
    };
    match output.status.code() {
        // No such setting anywhere.
        Some(1) => false,
        // Each setting's line starts with its scope and a tab. A value with a line break in it
        // can only add lines, so no setting of the repository's goes unseen.
        Some(0) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.starts_with("local\t") || line.starts_with("worktree\t")),
        _ => true,
    }
}

/// The branch, or `detached HEAD at <commit>`, read from the `HEAD` file of the repository at
/// `root` without running git; `None` when that file is anything but the simple case (a
/// `.git` file naming the git directory is followed, as for a linked worktree).
fn head_from_file(root: &Path) -> Option<String> {
    let dotgit = root.join(".git");
    let gitdir = if std::fs::metadata(&dotgit).ok()?.is_dir() {
        dotgit
    } else {
        let text = small_file(&dotgit)?;
        root.join(text.lines().next()?.strip_prefix("gitdir:")?.trim())
    };
    let head = small_file(&gitdir.join("HEAD"))?;
    let head = head.trim_end();
    if let Some(reference) = head.strip_prefix("ref: ") {
        // A reftable repository keeps a placeholder here.
        let branch = reference.strip_prefix("refs/heads/")?;
        return (!branch.is_empty() && branch != ".invalid").then(|| branch.to_string());
    }
    let hex = head.bytes().all(|b| b.is_ascii_hexdigit());
    (hex && matches!(head.len(), 40 | 64)).then(|| format!("detached HEAD at {}", &head[..7]))
}

/// The text of the regular file at `path`, at most 4 KiB of it.
fn small_file(path: &Path) -> Option<String> {
    let bytes = read_regular(path, 4096).ok()?;
    String::from_utf8(bytes).ok()
}

impl Environment {
    /// The environment section of the system prompt.
    pub fn render(&self) -> String {
        let mut out = format!(
            "# Environment\n\nWorking directory: {}\nOperating system: {}\nDate: {}\n",
            self.cwd.display(),
            self.os,
            self.date
        );
        if self.os == "macos" {
            out.push_str(MACOS_COMMIT_NOTE);
        }
        if let Some(git) = &self.git {
            if let Some(head) = &git.head {
                out.push_str(&format!("Git branch: {head}\n"));
            }
            match git.dirty {
                Some(true) => out.push_str("Uncommitted changes: yes\n"),
                Some(false) => out.push_str("Uncommitted changes: no\n"),
                None => {}
            }
        }
        out
    }
}
