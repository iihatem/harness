//! Facts about where the session runs, captured once when it starts.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use harness_core::subprocess::output_within;

use crate::project::repo_root;

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

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args);
    let output = output_within(&mut command, GIT_TIMEOUT).ok()??;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string()
    })
}

fn git_state(cwd: &Path) -> GitState {
    let head = git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
        .filter(|branch| !branch.is_empty())
        .or_else(|| {
            git(cwd, &["rev-parse", "--short", "HEAD"])
                .map(|commit| format!("detached HEAD at {commit}"))
        });
    let dirty = git(cwd, &["--no-optional-locks", "status", "--porcelain"])
        .map(|status| !status.is_empty());
    GitState { head, dirty }
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
