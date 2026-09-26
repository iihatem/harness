//! Decides whether each action may run, needs approval, or is refused, from the approval mode, the
//! user's rules, the shell-command analysis, path boundaries, and whether an OS sandbox exists.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use harness_shell::{Rules, Verdict};

use crate::permission::{Action, Decision, Mode, PermissionPolicy, resolve_path};

const TOOLS: [&str; 3] = ["bash", "read", "write"];

/// Rules as written in config: `<tool>:<glob>` strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleSet {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub confirm: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub mode: Mode,
    pub workspace: PathBuf,
    pub read_dirs: Vec<PathBuf>,
    pub rules: RuleSet,
    pub sandbox_available: bool,
}

pub struct PermissionEngine {
    mode: Mode,
    workspace: PathBuf,
    read_dirs: Vec<PathBuf>,
    rules: RuleSet,
    sandbox_available: bool,
    /// Allow rules added by approve-for-session.
    session: Mutex<Vec<String>>,
}

/// Absolute, symlink-resolved form of `p` (which may not exist yet).
fn resolved(p: &Path) -> PathBuf {
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    resolve_path(Path::new("/"), &absolute)
}

/// Globs of `list` that apply to `tool`, with the `tool:` prefix removed.
fn patterns<'a>(list: &'a [String], tool: &str) -> Vec<&'a str> {
    list.iter()
        .filter_map(|rule| rule.strip_prefix(tool)?.strip_prefix(':'))
        .collect()
}

fn short(command: &str) -> String {
    let line = command.lines().next().unwrap_or_default();
    let mut shown: String = line.chars().take(120).collect();
    if shown.len() < command.len() {
        shown.push('…');
    }
    shown
}

impl PermissionEngine {
    pub fn new(config: EngineConfig) -> Self {
        PermissionEngine {
            mode: config.mode,
            workspace: resolved(&config.workspace),
            read_dirs: config.read_dirs.iter().map(|d| resolved(d)).collect(),
            rules: config.rules,
            sandbox_available: config.sandbox_available,
            session: Mutex::new(Vec::new()),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Rules whose tool is not `bash`, `read`, or `write` (they never match; the CLI warns about them).
    pub fn unknown_rules(&self) -> Vec<String> {
        [&self.rules.allow, &self.rules.deny, &self.rules.confirm]
            .into_iter()
            .flatten()
            .filter(|rule| {
                let tool = rule.split(':').next().unwrap_or_default();
                !rule.contains(':') || !TOOLS.contains(&tool)
            })
            .cloned()
            .collect()
    }

    fn allow_list(&self) -> Vec<String> {
        let session = self.session.lock().expect("session rules lock");
        self.rules
            .allow
            .iter()
            .chain(session.iter())
            .cloned()
            .collect()
    }

    /// First `tool:` glob in `list` matching the path (workspace-relative or absolute text).
    fn path_rule(&self, list: &[String], tool: &str, target: &Path) -> Option<String> {
        let absolute = target.display().to_string();
        let relative = target
            .strip_prefix(&self.workspace)
            .ok()
            .map(|r| r.display().to_string());
        patterns(list, tool)
            .into_iter()
            .find(|glob| {
                harness_shell::glob_match(glob, &absolute)
                    || relative
                        .as_deref()
                        .is_some_and(|r| harness_shell::glob_match(glob, r))
            })
            .map(|glob| format!("{tool}:{glob}"))
    }

    fn check_read(&self, path: &Path) -> Decision {
        let target = resolve_path(&self.workspace, path);
        if let Some(rule) = self.path_rule(&self.rules.deny, "read", &target) {
            return Decision::Deny(format!("denied by rule `{rule}`"));
        }
        if self.mode == Mode::FullAccess
            || target.starts_with(&self.workspace)
            || self.read_dirs.iter().any(|d| target.starts_with(d))
            || self
                .path_rule(&self.allow_list(), "read", &target)
                .is_some()
        {
            return Decision::Allow;
        }
        Decision::Ask(format!("read outside the workspace: {}", target.display()))
    }

    fn check_write(&self, path: &Path) -> Decision {
        let target = resolve_path(&self.workspace, path);
        if let Some(rule) = self.path_rule(&self.rules.deny, "write", &target) {
            return Decision::Deny(format!("denied by rule `{rule}`"));
        }
        if self.mode == Mode::FullAccess {
            return Decision::Allow;
        }
        if matches!(self.mode, Mode::Plan | Mode::ReadOnly) {
            return Decision::Deny(format!("file writes are not allowed in {} mode", self.mode));
        }
        let Ok(inside) = target.strip_prefix(&self.workspace) else {
            return Decision::Ask(format!("write outside the workspace: {}", target.display()));
        };
        if inside.components().any(|c| c.as_os_str() == ".git") {
            return Decision::Ask("write inside .git (hooks and config can run commands)".into());
        }
        if let Some(rule) = self.path_rule(&self.rules.confirm, "write", &target) {
            return Decision::Ask(format!(
                "write {} (confirm rule `{rule}`)",
                inside.display()
            ));
        }
        if self.mode == Mode::Auto
            || self
                .path_rule(&self.allow_list(), "write", &target)
                .is_some()
        {
            return Decision::Allow;
        }
        Decision::Ask(format!("write {}", inside.display()))
    }

    fn check_bash(&self, command: &str) -> Decision {
        let allow = self.allow_list();
        let own = |v: Vec<&str>| v.into_iter().map(str::to_string).collect();
        let rules = Rules {
            allow: own(patterns(&allow, "bash")),
            deny: own(patterns(&self.rules.deny, "bash")),
            confirm: own(patterns(&self.rules.confirm, "bash")),
        };
        match harness_shell::evaluate(command, &rules, &self.workspace) {
            Verdict::Deny { reason } => Decision::Deny(reason),
            _ if self.mode == Mode::FullAccess => Decision::Allow,
            _ if !self.sandbox_available => Decision::Ask(format!(
                "run `{}` (no sandbox is available on this system)",
                short(command)
            )),
            Verdict::Ask { reason, .. } => Decision::Ask(reason),
            Verdict::Allow => Decision::Allow,
            Verdict::Unlisted if self.mode == Mode::Ask => {
                Decision::Ask(format!("run `{}`", short(command)))
            }
            Verdict::Unlisted => Decision::Allow,
        }
    }
}

impl PermissionPolicy for PermissionEngine {
    fn check(&self, action: &Action) -> Decision {
        match action {
            Action::Read(path) => self.check_read(path),
            Action::Write(path) => self.check_write(path),
            Action::Bash(command) => self.check_bash(command),
        }
    }

    fn remember(&self, action: &Action) -> bool {
        let rules: Vec<String> = match action {
            Action::Bash(command) => match harness_shell::session_prefixes(command) {
                Some(prefixes) if !prefixes.is_empty() => {
                    prefixes.iter().map(|p| format!("bash:{p} *")).collect()
                }
                _ => return false,
            },
            Action::Write(path) => {
                let target = resolve_path(&self.workspace, path);
                if !target.starts_with(&self.workspace) {
                    return false;
                }
                vec![format!("write:{}", target.display())]
            }
            Action::Read(path) => {
                vec![format!(
                    "read:{}",
                    resolve_path(&self.workspace, path).display()
                )]
            }
        };
        self.session
            .lock()
            .expect("session rules lock")
            .extend(rules);
        true
    }
}
