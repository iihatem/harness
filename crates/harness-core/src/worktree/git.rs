//! Running `git` for harness's own purposes: in harness's process, outside any sandbox, with
//! what the repository's configuration could make it run switched off.
//!
//! A repository is data an agent (or an untrusted checkout) may have written, so a hook, a
//! filter driver, a filesystem monitor, an external diff or a merge driver in it must not run
//! when harness makes a worktree, commits an agent's result or applies it. Every command gets
//! `core.hooksPath` pointed at nothing, `core.fsmonitor` off, signing off, every configured
//! `filter.*` command replaced with `cat` (LFS files then arrive as pointers) and every merge
//! driver emptied (a file that needs one conflicts); diffs are asked for with `--no-ext-diff
//! --no-textconv` by the callers. Variables that would send git somewhere else are removed.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Variables that would point git at another repository, index, editor or program.
const REMOVED_ENV: [&str; 14] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_EXTERNAL_DIFF",
    "GIT_SEQUENCE_EDITOR",
    "GIT_ASKPASS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Settings given to every command, whatever the repository says.
const FIXED: [&str; 8] = [
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "commit.gpgsign=false",
    "tag.gpgsign=false",
    "gc.auto=0",
    "maintenance.auto=false",
    "protocol.ext.allow=never",
    "advice.detachedHead=false",
];

/// A git command that did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("git {args} failed{}: {stderr}", code.map(|c| format!(" (exit {c})")).unwrap_or_default())]
pub struct GitError {
    /// The command's arguments, as far as the subcommand and its first option.
    pub args: String,
    pub code: Option<i32>,
    /// What git said, trimmed.
    pub stderr: String,
}

/// Runs git in one directory with the repository's own code switched off. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Git {
    cwd: PathBuf,
    /// `-c name=value` for each filter and merge driver the repository configures.
    overrides: Vec<String>,
}

impl Git {
    /// A runner for the repository (or worktree) at `dir`. Fails when `git` cannot be run there.
    pub fn open(dir: &Path) -> Result<Git, GitError> {
        let mut git = Git {
            cwd: dir.to_path_buf(),
            overrides: Vec::new(),
        };
        let configured = git.run(&[
            "config",
            "--null",
            "--get-regexp",
            r"^(filter\..*\.(clean|smudge|process|required)|merge\..*\.driver)$",
        ]);
        // Exit 1 is "no such key": nothing to override.
        let text = match configured {
            Ok(text) => text,
            Err(e) if e.code == Some(1) => String::new(),
            Err(e) => return Err(e),
        };
        for entry in text.split('\0').filter(|e| !e.is_empty()) {
            let key = entry.split('\n').next().unwrap_or(entry);
            let value = if key.starts_with("merge.") {
                ""
            } else if key.ends_with(".clean") || key.ends_with(".smudge") {
                "cat"
            } else if key.ends_with(".required") {
                "false"
            } else {
                ""
            };
            git.overrides.push(format!("{key}={value}"));
        }
        Ok(git)
    }

    /// The same runner for another directory of the same repository (a worktree).
    pub fn at(&self, dir: &Path) -> Git {
        Git {
            cwd: dir.to_path_buf(),
            overrides: self.overrides.clone(),
        }
    }

    /// The directory the commands run in.
    pub fn dir(&self) -> &Path {
        &self.cwd
    }

    /// The command for `git args`, not yet run.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.arg("--no-pager");
        for setting in FIXED
            .iter()
            .map(|s| s.to_string())
            .chain(self.overrides.iter().cloned())
        {
            command.arg("-c").arg(setting);
        }
        command
            .args(args)
            .current_dir(&self.cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        for name in REMOVED_ENV {
            command.env_remove(name);
        }
        command
    }

    fn failure(args: &[&str], code: Option<i32>, stderr: &[u8]) -> GitError {
        GitError {
            args: args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            code,
            stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        }
    }

    /// Runs `git args` and returns its output.
    pub fn run(&self, args: &[&str]) -> Result<String, GitError> {
        self.run_with_env(args, &[])
    }

    /// [`run`](Self::run) with more environment variables.
    pub fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String, GitError> {
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        let output = command
            .output()
            .map_err(|e| Self::failure(args, None, e.to_string().as_bytes()))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(Self::failure(args, output.status.code(), &output.stderr))
        }
    }

    /// Runs `git args` and returns at most `cap` bytes of its output and whether there was more
    /// (the command is then stopped).
    pub fn run_capped(&self, args: &[&str], cap: usize) -> Result<(String, bool), GitError> {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Self::failure(args, None, e.to_string().as_bytes()))?;
        let mut out = Vec::new();
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let _ = (&mut stdout).take(cap as u64 + 1).read_to_end(&mut out);
        let cut = out.len() > cap;
        if cut {
            out.truncate(cap);
            let _ = child.kill();
        }
        drop(stdout);
        let status = child
            .wait()
            .map_err(|e| Self::failure(args, None, e.to_string().as_bytes()))?;
        if !cut && !status.success() {
            return Err(Self::failure(args, status.code(), b""));
        }
        // A cut may end inside a character.
        Ok((String::from_utf8_lossy(&out).into_owned(), cut))
    }

    /// The user's name and email from their git configuration, when both are set.
    pub fn identity(&self) -> Option<(String, String)> {
        let get = |key: &str| {
            self.run(&["config", "--get", key])
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Some((get("user.name")?, get("user.email")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testing::{git as plain, init_repo, marking_script, write};

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        dir
    }

    #[test]
    fn a_hook_of_the_repository_does_not_run() {
        let dir = repo();
        let marker = dir.path().join("hook-ran");
        marking_script(&dir.path().join(".git/hooks/pre-commit"), &marker);
        marking_script(&dir.path().join(".git/hooks/post-checkout"), &marker);
        marking_script(&dir.path().join(".git/hooks/post-commit"), &marker);
        write(dir.path(), "a.txt", "a\n");
        let git = Git::open(dir.path()).unwrap();
        git.run(&["add", "-A"]).unwrap();
        git.run(&["commit", "-q", "-m", "x"]).unwrap();
        git.run(&["worktree", "add", "-q", "-b", "side", "../side-tree"])
            .unwrap();
        let side = dir.path().join("../side-tree");
        assert!(side.join("a.txt").exists(), "the worktree was made");
        assert!(!marker.exists(), "a hook ran");
        // The hooks are there and a plain git runs them: the test would see it.
        write(dir.path(), "b.txt", "b\n");
        plain(dir.path(), &["add", "-A"]);
        plain(dir.path(), &["commit", "-q", "-m", "y"]);
        assert!(marker.exists(), "the fixture's hooks work");
        std::fs::remove_dir_all(side).unwrap();
    }

    #[test]
    fn a_filter_driver_of_the_repository_does_not_run() {
        let dir = repo();
        let marker = dir.path().join("filter-ran");
        let script = dir.path().join(".git/filter.sh");
        marking_script(&script, &marker);
        plain(
            dir.path(),
            &["config", "filter.evil.clean", &script.display().to_string()],
        );
        plain(
            dir.path(),
            &[
                "config",
                "filter.evil.smudge",
                &script.display().to_string(),
            ],
        );
        plain(
            dir.path(),
            &[
                "config",
                "filter.evil.process",
                &script.display().to_string(),
            ],
        );
        write(dir.path(), ".gitattributes", "*.txt filter=evil\n");
        write(dir.path(), "a.txt", "a\n");
        let git = Git::open(dir.path()).unwrap();
        git.run(&["add", "-A"]).unwrap();
        git.run(&["commit", "-q", "-m", "x"]).unwrap();
        git.run(&["worktree", "add", "-q", "-b", "side", "../side-tree2"])
            .unwrap();
        assert!(!marker.exists(), "a filter driver ran");
        std::fs::remove_dir_all(dir.path().join("../side-tree2")).unwrap();
    }

    #[test]
    fn a_filesystem_monitor_of_the_repository_does_not_run() {
        let dir = repo();
        let marker = dir.path().join("fsmonitor-ran");
        let script = dir.path().join(".git/monitor.sh");
        marking_script(&script, &marker);
        plain(
            dir.path(),
            &["config", "core.fsmonitor", &script.display().to_string()],
        );
        write(dir.path(), "a.txt", "a\n");
        Git::open(dir.path())
            .unwrap()
            .run(&["status", "--porcelain"])
            .unwrap();
        assert!(!marker.exists(), "the monitor ran");
    }

    #[test]
    fn the_users_identity_is_read_from_their_configuration() {
        let dir = repo();
        plain(dir.path(), &["config", "user.name", "Ada Lovelace"]);
        plain(dir.path(), &["config", "user.email", "ada@example.org"]);
        let who = Git::open(dir.path()).unwrap().identity();
        assert_eq!(
            who,
            Some(("Ada Lovelace".to_string(), "ada@example.org".to_string()))
        );
    }

    #[test]
    fn variables_that_would_redirect_git_are_removed_from_every_command() {
        let dir = repo();
        let git = Git::open(dir.path()).unwrap();
        let command = git.command(&["status"]);
        let removed: Vec<_> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_EXTERNAL_DIFF",
        ] {
            assert!(removed.iter().any(|r| r == name), "{name} not removed");
        }
    }

    #[test]
    fn a_failing_command_says_what_git_said() {
        let dir = repo();
        let error = Git::open(dir.path())
            .unwrap()
            .run(&["rev-parse", "--verify", "nope"])
            .unwrap_err();
        assert!(error.to_string().contains("rev-parse"), "{error}");
        assert!(error.code.is_some());
    }

    #[test]
    fn output_is_cut_at_the_cap() {
        let dir = repo();
        write(dir.path(), "big.txt", &"line of text\n".repeat(5000));
        let git = Git::open(dir.path()).unwrap();
        git.run(&["add", "-A"]).unwrap();
        let (text, cut) = git
            .run_capped(&["diff", "--cached", "--no-ext-diff"], 1000)
            .unwrap();
        assert!(cut);
        assert!(text.len() <= 1000);
        let (whole, cut) = git.run_capped(&["status", "--porcelain"], 1000).unwrap();
        assert!(!cut);
        assert!(whole.contains("big.txt"));
    }
}
