//! Running `git` for harness's own purposes: in harness's process, outside any sandbox, with
//! what the repository's configuration could make it run switched off.
//!
//! A repository is data an agent (or an untrusted checkout) may have written, so a hook, a
//! filter driver, a filesystem monitor, an external diff, a text conversion or a merge driver in
//! it must not run when harness makes a worktree, commits an agent's result or applies it.
//! Every command gets `core.hooksPath` pointed at nothing, `core.fsmonitor` off, signing off,
//! every configured `filter.*` command replaced with `cat` (LFS files then arrive as pointers),
//! every `diff.*.textconv` replaced with `cat` and every merge driver emptied (a file that needs
//! one conflicts). The settings are given through git's `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` /
//! `GIT_CONFIG_VALUE_n` environment, where a key is taken whole: a driver named `a=b` or `a.b` is
//! switched off like any other (`-c key=value` splits at the first `=`). After they are worked
//! out, git is asked what it reads with them in place, and when a driver would still be
//! configured the command is refused rather than run. Every `GIT_*` variable of harness's own
//! environment is dropped, so nothing in it can send git elsewhere or switch a setting back on.

use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Variables that are removed even when harness's environment does not have them: the ones that
/// would point git at another repository, index, editor or program, or at other configuration.
/// (Every other variable starting with `GIT_` is removed too.)
const REMOVED_ENV: [&str; 24] = [
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
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_EXEC_PATH",
    "GIT_ATTR_SOURCE",
    "GIT_PROXY_COMMAND",
    "GIT_SSL_NO_VERIFY",
    "GIT_TEMPLATE_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// Settings given to every command, whatever the repository says.
const FIXED: [(&str, &str); 11] = [
    ("core.hooksPath", "/dev/null"),
    ("core.fsmonitor", "false"),
    ("core.pager", "cat"),
    ("core.editor", "true"),
    ("sequence.editor", "true"),
    ("commit.gpgsign", "false"),
    ("tag.gpgsign", "false"),
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
    ("protocol.ext.allow", "never"),
    ("advice.detachedHead", "false"),
];

/// The keys whose commands are replaced: filter drivers, merge drivers, text conversions and
/// external diff drivers.
const DRIVER_KEYS: &str = r"^(filter\..*\.(clean|smudge|process|required)|merge\..*\.driver|diff\..*\.(textconv|command))$";

/// The names of the variables harness's own environment has.
fn inherited() -> Vec<OsString> {
    std::env::vars_os().map(|(name, _)| name).collect()
}

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
    /// (key, value) for each driver the repository configures, as the configuration was when
    /// this runner was made.
    overrides: Vec<(String, String)>,
}

/// What replaces a driver's command, by the setting's key; `None` when the key is not one of
/// the driver settings.
fn replacement(key: &str) -> Option<&'static str> {
    let ends = |tail: &str| key.ends_with(tail);
    if key.starts_with("merge.") && ends(".driver") {
        Some("")
    } else if key.starts_with("filter.") && (ends(".clean") || ends(".smudge")) {
        Some("cat")
    } else if key.starts_with("filter.") && ends(".required") {
        Some("false")
    } else if key.starts_with("filter.") && ends(".process") {
        Some("")
    } else if key.starts_with("diff.") && ends(".textconv") {
        Some("cat")
    } else if key.starts_with("diff.") && ends(".command") {
        Some("true")
    } else {
        None
    }
}

/// The (key, value) pairs of `git config --null --get-regexp` output.
fn parse_listing(text: &str) -> Vec<(String, String)> {
    text.split('\0')
        .filter(|e| !e.is_empty())
        .map(|entry| match entry.split_once('\n') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (entry.to_string(), String::new()),
        })
        .collect()
}

impl Git {
    /// A runner for the repository (or worktree) at `dir`. Fails when `git` cannot be run there,
    /// or when a driver of its configuration cannot be switched off.
    pub fn open(dir: &Path) -> Result<Git, GitError> {
        Ok(Git {
            cwd: dir.to_path_buf(),
            overrides: Self::discover(dir)?,
        })
    }

    fn listing(
        dir: &Path,
        overrides: &[(String, String)],
    ) -> Result<Vec<(String, String)>, GitError> {
        let args = ["config", "--null", "--get-regexp", DRIVER_KEYS];
        let output = Self::build(dir, overrides, &args, &inherited())
            .output()
            .map_err(|e| Self::failure(&args, None, e.to_string().as_bytes()))?;
        // Exit 1 is "no such key": nothing to override.
        match output.status.code() {
            Some(0) => Ok(parse_listing(&String::from_utf8_lossy(&output.stdout))),
            Some(1) => Ok(Vec::new()),
            code => Err(Self::failure(&args, code, &output.stderr)),
        }
    }

    /// The settings that switch off the drivers the configuration of the repository at `dir` has
    /// now. Fails closed: when git, asked again with them in place, still reports a driver that
    /// is not switched off, nothing is run.
    fn discover(dir: &Path) -> Result<Vec<(String, String)>, GitError> {
        let mut overrides: Vec<(String, String)> = Vec::new();
        for (key, _) in Self::listing(dir, &[])? {
            if let Some(value) = replacement(&key)
                && !overrides.iter().any(|(k, _)| *k == key)
            {
                overrides.push((key, value.to_string()));
            }
        }
        let after = Self::listing(dir, &overrides)?;
        Self::verify(&overrides, &after)?;
        Ok(overrides)
    }

    /// Whether `after`, what git reports with `overrides` in place, shows every driver switched
    /// off: the last value of each key is the override's, and no other driver key is left.
    fn verify(overrides: &[(String, String)], after: &[(String, String)]) -> Result<(), GitError> {
        let not_off = |key: &str| {
            Self::refused(&format!(
                "could not switch off the repository's setting {key:?}, so git was not run"
            ))
        };
        for (key, value) in overrides {
            match after.iter().rev().find(|(k, _)| k == key) {
                Some((_, last)) if last == value => {}
                _ => return Err(not_off(key)),
            }
        }
        match after
            .iter()
            .find(|(key, _)| !overrides.iter().any(|(k, _)| k == key))
        {
            Some((key, _)) => Err(not_off(key)),
            None => Ok(()),
        }
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

    /// The command for `git args`, not yet run, as the configuration was when this runner was
    /// made ([`run`](Self::run) reads it again each time).
    #[cfg(test)]
    pub fn command(&self, args: &[&str]) -> Command {
        Self::build(&self.cwd, &self.overrides, args, &inherited())
    }

    /// The command for the configuration as it is now: a driver added to it since is switched
    /// off as well. Fails when one cannot be.
    fn fresh(&self, args: &[&str]) -> Result<Command, GitError> {
        let overrides = Self::discover(&self.cwd)?;
        Ok(Self::build(&self.cwd, &overrides, args, &inherited()))
    }

    /// `git args` in `cwd` with the fixed settings and `overrides` in its environment, and
    /// `inherited` (the names of the variables harness's own environment has) removed from it.
    fn build(
        cwd: &Path,
        overrides: &[(String, String)],
        args: &[&str],
        inherited: &[OsString],
    ) -> Command {
        let mut command = Command::new("git");
        command.arg("--no-pager").args(args).current_dir(cwd);
        for name in inherited {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        for name in REMOVED_ENV {
            command.env_remove(name);
        }
        let settings: Vec<(&str, &str)> = FIXED
            .iter()
            .copied()
            .chain(overrides.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .collect();
        command.env("GIT_CONFIG_COUNT", settings.len().to_string());
        for (n, (key, value)) in settings.iter().enumerate() {
            command.env(format!("GIT_CONFIG_KEY_{n}"), key);
            command.env(format!("GIT_CONFIG_VALUE_{n}"), value);
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        command
    }

    fn failure(args: &[&str], code: Option<i32>, stderr: &[u8]) -> GitError {
        GitError {
            args: args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
            code,
            stderr: String::from_utf8_lossy(stderr).trim().to_string(),
        }
    }

    fn refused(why: &str) -> GitError {
        GitError {
            args: "config".into(),
            code: None,
            stderr: why.to_string(),
        }
    }

    /// Runs `git args` and returns its output.
    pub fn run(&self, args: &[&str]) -> Result<String, GitError> {
        self.run_with_env(args, &[])
    }

    /// [`run`](Self::run) with more environment variables.
    pub fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String, GitError> {
        let mut command = self.fresh(args)?;
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
            .fresh(args)?
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
        let elsewhere = tempfile::tempdir().unwrap();
        let side = elsewhere.path().join("side-tree");
        git.run(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            side.to_str().unwrap(),
        ])
        .unwrap();
        assert!(side.join("a.txt").exists(), "the worktree was made");
        assert!(!marker.exists(), "a hook ran");
        // The hooks are there and a plain git runs them: the test would see it.
        write(dir.path(), "b.txt", "b\n");
        plain(dir.path(), &["add", "-A"]);
        plain(dir.path(), &["commit", "-q", "-m", "y"]);
        assert!(marker.exists(), "the fixture's hooks work");
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
        let elsewhere = tempfile::tempdir().unwrap();
        let side = elsewhere.path().join("side-tree");
        git.run(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            side.to_str().unwrap(),
        ])
        .unwrap();
        assert!(!marker.exists(), "a filter driver ran");
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
        let removed = |command: &Command| -> Vec<String> {
            command
                .get_envs()
                .filter(|(_, value)| value.is_none())
                .map(|(key, _)| key.to_string_lossy().into_owned())
                .collect()
        };
        let removed_plain = removed(&git.command(&["status"]));
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_EXTERNAL_DIFF",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXEC_PATH",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
        ] {
            assert!(
                removed_plain.iter().any(|r| r == name),
                "{name} not removed"
            );
        }
        // Whatever else the environment has that starts with GIT_ goes too, and nothing else.
        let inherited: Vec<OsString> = ["GIT_SOMETHING_NEW", "GIT_CONFIG_KEY_7", "HOME", "PATH"]
            .iter()
            .map(OsString::from)
            .collect();
        let command = Git::build(dir.path(), &[], &["status"], &inherited);
        let gone = removed(&command);
        assert!(gone.iter().any(|r| r == "GIT_SOMETHING_NEW"), "{gone:?}");
        assert!(
            gone.iter().any(|r| r == "GIT_CONFIG_KEY_7")
                || command
                    .get_envs()
                    .any(|(k, v)| k == "GIT_CONFIG_KEY_7" && v.is_some())
        );
        assert!(!gone.iter().any(|r| r == "HOME" || r == "PATH"), "{gone:?}");
    }

    #[test]
    fn the_settings_are_passed_through_the_environment_and_not_as_dash_c() {
        let dir = repo();
        let overrides = vec![("filter.a=b.clean".to_string(), "cat".to_string())];
        let command = Git::build(dir.path(), &overrides, &["status"], &[]);
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a == "-c"), "{args:?}");
        let env = |name: &str| {
            command
                .get_envs()
                .find(|(k, _)| *k == name)
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        let count: usize = env("GIT_CONFIG_COUNT").unwrap().parse().unwrap();
        assert_eq!(count, FIXED.len() + 1);
        let last = count - 1;
        assert_eq!(
            env(&format!("GIT_CONFIG_KEY_{last}")).as_deref(),
            Some("filter.a=b.clean")
        );
        assert_eq!(
            env(&format!("GIT_CONFIG_VALUE_{last}")).as_deref(),
            Some("cat")
        );
    }

    #[test]
    fn a_driver_git_would_still_run_is_refused_not_run() {
        let off = |k: &str, v: &str| (k.to_string(), v.to_string());
        let overrides = vec![off("filter.x.clean", "cat")];
        assert!(Git::verify(&overrides, &[off("filter.x.clean", "cat")]).is_ok());
        // The repository's value is still the last one.
        let stale = [off("filter.x.clean", "cat"), off("filter.x.clean", "evil")];
        assert!(Git::verify(&overrides, &stale).is_err());
        // The setting was not applied at all.
        assert!(Git::verify(&overrides, &[]).is_err());
        // A driver that was not overridden is there.
        let extra = [off("filter.x.clean", "cat"), off("merge.y.driver", "evil")];
        let error = Git::verify(&overrides, &extra).unwrap_err();
        assert!(error.to_string().contains("merge.y.driver"), "{error}");
    }

    /// Names a driver can have in a configuration: `=` and `.` split a `-c key=value`, spaces
    /// and quotes need escaping, and so on.
    const DRIVER_NAMES: [&str; 13] = [
        "evil", "a=b", "a.b", "a b", "a\"b", "a'b", "a;b", "a#b", "=", ".", "a=", "=a", "a\\b",
    ];

    fn driver_kinds(dir: &Path, name: &str, script: &Path) {
        let script = script.display().to_string();
        for key in ["clean", "smudge", "process"] {
            plain(dir, &["config", &format!("filter.{name}.{key}"), &script]);
        }
        plain(dir, &["config", &format!("filter.{name}.required"), "true"]);
        plain(dir, &["config", &format!("merge.{name}.driver"), &script]);
        plain(dir, &["config", &format!("diff.{name}.textconv"), &script]);
        plain(dir, &["config", &format!("diff.{name}.command"), &script]);
    }

    #[test]
    fn no_driver_runs_whatever_it_is_called() {
        for name in DRIVER_NAMES {
            let dir = repo();
            let marker = dir.path().join("canary");
            let script = dir.path().join(".git/driver.sh");
            marking_script(&script, &marker);
            // Committed before any driver is configured, so that plain git does not run them.
            write(
                dir.path(),
                ".gitattributes",
                &format!("*.txt filter={name} merge={name} diff={name}\n"),
            );
            write(dir.path(), "a.txt", "a\n");
            plain(dir.path(), &["add", "-A"]);
            plain(dir.path(), &["commit", "-q", "-m", "attributes"]);
            plain(dir.path(), &["checkout", "-q", "-b", "side"]);
            write(dir.path(), "a.txt", "side\n");
            plain(dir.path(), &["commit", "-q", "-am", "side"]);
            plain(dir.path(), &["checkout", "-q", "main"]);
            write(dir.path(), "a.txt", "main\n");
            plain(dir.path(), &["commit", "-q", "-am", "main"]);
            driver_kinds(dir.path(), name, &script);

            let git = Git::open(dir.path()).unwrap_or_else(|e| panic!("{name:?}: {e}"));
            let elsewhere = tempfile::tempdir().unwrap();
            let side = elsewhere.path().join("side-tree");
            // worktree add: a checkout, which smudges.
            let _ = git.run(&["worktree", "add", "-q", "-b", "wt", side.to_str().unwrap()]);
            // commit: add (clean), diff (textconv), commit.
            write(dir.path(), "a.txt", "changed\n");
            write(dir.path(), "b.txt", "b\n");
            let _ = git.run(&["add", "-A"]);
            let _ = git.run(&["diff", "--cached", "--quiet"]);
            let _ = git.run(&["diff", "--cached"]);
            let _ = git.run(&["diff", "--cached", "--stat"]);
            let _ = git.run(&["diff"]);
            let _ = git.run(&["commit", "-q", "--no-verify", "-m", "x"]);
            // cherry-pick: a merge, which uses merge drivers and filters.
            let _ = git.run(&["reset", "-q", "--hard", "HEAD~1"]);
            let _ = git.run(&["cherry-pick", "side"]);
            let _ = git.run(&["cherry-pick", "--abort"]);
            let _ = git.run(&["status", "--porcelain"]);
            assert!(
                !marker.exists(),
                "a driver of the repository ran for the name {name:?}"
            );

            // The same repository under plain git does run it (a filter, at least), so the
            // test could have seen it.
            if !name.contains(char::is_whitespace) {
                write(dir.path(), "c.txt", "c\n");
                let _ = std::process::Command::new("git")
                    .args(["add", "-A"])
                    .current_dir(dir.path())
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .stdin(Stdio::null())
                    .output();
                assert!(marker.exists(), "the fixture's driver works for {name:?}");
            }
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
