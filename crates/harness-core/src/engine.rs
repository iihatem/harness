//! Decides whether each action may run, needs approval, or is refused, from the approval mode, the
//! user's rules, the shell-command analysis, path boundaries, and whether an OS sandbox exists.

use std::{
    collections::HashSet,
    ffi::OsStr,
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
    /// Bash allow-glob prefixes added by approve-for-session.
    session_bash: Mutex<Vec<String>>,
    /// Exact (tool, resolved path) approvals added by approve-for-session. Stored as exact
    /// paths rather than globs, so a literal `*` in an approved path is never treated as a
    /// wildcard over its siblings (a `glob_match` pattern has no escape syntax).
    session_paths: Mutex<HashSet<(&'static str, PathBuf)>>,
}

/// Absolute, symlink-resolved form of `p` (which may not exist yet). If `p` is relative and
/// the current directory can't be read, `p` is returned unresolved rather than guessed at
/// against `/`: an unresolved relative path never equals a resolved absolute target, so every
/// check involving it safely falls through to asking instead of risking a wrong match.
fn resolved(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return resolve_path(Path::new("/"), p);
    }
    match std::env::current_dir() {
        Ok(cwd) => resolve_path(Path::new("/"), &cwd.join(p)),
        Err(_) => p.to_path_buf(),
    }
}

/// `$HOME`, if set.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Globs of `list` that apply to `tool`, with the `tool:` prefix removed.
fn patterns<'a>(list: &'a [String], tool: &str) -> Vec<&'a str> {
    list.iter()
        .filter_map(|rule| rule.strip_prefix(tool)?.strip_prefix(':'))
        .collect()
}

fn own(v: Vec<&str>) -> Vec<String> {
    v.into_iter().map(str::to_string).collect()
}

/// Whether `component` is `.git`, case-insensitively (macOS's default filesystem is
/// case-insensitive, so `.GIT` reaches the same directory as `.git`).
fn is_dot_git(component: &OsStr) -> bool {
    component
        .to_str()
        .is_some_and(|s| s.eq_ignore_ascii_case(".git"))
}

/// Resolves the literal directory prefix of an absolute glob — the text before its first `*`,
/// cut at the last `/` — through symlinks, so a rule written against e.g. `/etc/*` still
/// matches the resolved path the engine actually compares targets against (macOS resolves
/// `/etc` to `/private/etc`). Returns `None` when there's no prefix worth resolving (the glob
/// covers the filesystem root) or the resolved prefix is unchanged.
fn resolve_glob_prefix(glob: &str) -> Option<String> {
    let cut = glob.find('*').unwrap_or(glob.len());
    let prefix_end = glob[..cut].rfind('/')?;
    if prefix_end == 0 {
        return None; // the root itself can't be a symlink
    }
    let (dir, rest) = glob.split_at(prefix_end);
    let resolved = resolve_path(Path::new("/"), Path::new(dir))
        .display()
        .to_string();
    if resolved == dir {
        None
    } else {
        Some(format!("{resolved}{rest}"))
    }
}

/// Expands a leading `~/` using `$HOME`, then adds a symlink-resolved twin for any resulting
/// absolute `read:`/`write:` glob, so e.g. a rule on `/etc/*` still matches macOS's real
/// `/private/etc/*`, and `~/.ssh/*` matches the user's actual home directory. Leaves `bash:`
/// rules and rules for tools other than `read`/`write` untouched.
fn expand_path_rules(rules: &[String], home: Option<&Path>) -> Vec<String> {
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        let Some((tool, glob)) = rule.split_once(':') else {
            out.push(rule.clone());
            continue;
        };
        if tool != "read" && tool != "write" {
            out.push(rule.clone());
            continue;
        }
        let expanded = match (glob.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
            _ => glob.to_string(),
        };
        if expanded.starts_with('/')
            && let Some(resolved) = resolve_glob_prefix(&expanded)
        {
            out.push(format!("{tool}:{resolved}"));
        }
        out.push(format!("{tool}:{expanded}"));
    }
    out
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
        let home = home_dir();
        let rules = RuleSet {
            allow: expand_path_rules(&config.rules.allow, home.as_deref()),
            deny: expand_path_rules(&config.rules.deny, home.as_deref()),
            confirm: expand_path_rules(&config.rules.confirm, home.as_deref()),
        };
        PermissionEngine {
            mode: config.mode,
            workspace: resolved(&config.workspace),
            read_dirs: config.read_dirs.iter().map(|d| resolved(d)).collect(),
            rules,
            sandbox_available: config.sandbox_available,
            session_bash: Mutex::new(Vec::new()),
            session_paths: Mutex::new(HashSet::new()),
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

    /// Config allow rules plus bash prefixes added by approve-for-session.
    fn allow_list(&self) -> Vec<String> {
        let session = self.session_bash.lock().expect("session bash lock");
        self.rules
            .allow
            .iter()
            .chain(session.iter())
            .cloned()
            .collect()
    }

    /// First `tool:` glob in `list` matching the path (workspace-relative or absolute text).
    /// Deny and confirm rules match case-insensitively, since macOS's default filesystem is
    /// case-insensitive and a denied path must not be reachable by changing its case; allow
    /// rules stay case-sensitive, the stricter side.
    fn path_rule(
        &self,
        list: &[String],
        tool: &str,
        target: &Path,
        case_insensitive: bool,
    ) -> Option<String> {
        let fold = |s: &str| {
            if case_insensitive {
                s.to_lowercase()
            } else {
                s.to_string()
            }
        };
        let absolute = fold(&target.display().to_string());
        let relative = target
            .strip_prefix(&self.workspace)
            .ok()
            .map(|r| fold(&r.display().to_string()));
        patterns(list, tool)
            .into_iter()
            .find(|glob| {
                let folded = fold(glob);
                harness_shell::glob_match(&folded, &absolute)
                    || relative
                        .as_deref()
                        .is_some_and(|r| harness_shell::glob_match(&folded, r))
            })
            .map(|glob| format!("{tool}:{glob}"))
    }

    /// Whether `target` was approved for `tool` ("read" or "write") for the rest of the session.
    fn session_path_allowed(&self, tool: &'static str, target: &Path) -> bool {
        self.session_paths
            .lock()
            .expect("session paths lock")
            .contains(&(tool, target.to_path_buf()))
    }

    fn check_read(&self, path: &Path) -> Decision {
        let target = resolve_path(&self.workspace, path);
        if let Some(rule) = self.path_rule(&self.rules.deny, "read", &target, true) {
            return Decision::Deny(format!("denied by rule `{rule}`"));
        }
        if self.mode != Mode::FullAccess
            && let Some(rule) = self.path_rule(&self.rules.confirm, "read", &target, true)
        {
            return Decision::Ask(format!("read {} (confirm rule `{rule}`)", target.display()));
        }
        if self.mode == Mode::FullAccess
            || target.starts_with(&self.workspace)
            || self.read_dirs.iter().any(|d| target.starts_with(d))
            || self
                .path_rule(&self.rules.allow, "read", &target, false)
                .is_some()
            || self.session_path_allowed("read", &target)
        {
            return Decision::Allow;
        }
        Decision::Ask(format!("read outside the workspace: {}", target.display()))
    }

    fn check_write(&self, path: &Path) -> Decision {
        let target = resolve_path(&self.workspace, path);
        if let Some(rule) = self.path_rule(&self.rules.deny, "write", &target, true) {
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
        if inside.components().any(|c| is_dot_git(c.as_os_str())) {
            return Decision::Ask("write inside .git (hooks and config can run commands)".into());
        }
        if let Some(rule) = self.path_rule(&self.rules.confirm, "write", &target, true) {
            return Decision::Ask(format!(
                "write {} (confirm rule `{rule}`)",
                inside.display()
            ));
        }
        if self.mode == Mode::Auto
            || self
                .path_rule(&self.rules.allow, "write", &target, false)
                .is_some()
            || self.session_path_allowed("write", &target)
        {
            return Decision::Allow;
        }
        Decision::Ask(format!("write {}", inside.display()))
    }

    /// The `Rules` harness-shell needs for `command`: config rules plus session-approved bash
    /// prefixes, each filtered down to `bash:` patterns with the prefix removed.
    fn bash_rules(&self) -> Rules {
        let allow = self.allow_list();
        Rules {
            allow: own(patterns(&allow, "bash")),
            deny: own(patterns(&self.rules.deny, "bash")),
            confirm: own(patterns(&self.rules.confirm, "bash")),
        }
    }

    fn check_bash(&self, command: &str) -> Decision {
        let rules = self.bash_rules();
        match harness_shell::evaluate(command, &rules, &self.workspace) {
            Verdict::Deny { reason } => Decision::Deny(reason),
            // Deny rules must win in every mode, including full-access: a command that may
            // match one (a possible match, or an undecomposable command that could be hiding
            // one) still asks, even though a definite destructive- or confirm-only ask does not.
            Verdict::Ask {
                reason,
                may_deny: true,
                ..
            } if self.mode == Mode::FullAccess => Decision::Ask(reason),
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

    /// Remembers a bash command's session prefixes as allow rules. Returns `false` (and
    /// remembers nothing) when doing so could never change a later decision: without a sandbox
    /// every command still asks regardless of the allow-list; and harness-shell's own analysis
    /// (deny, destructive, confirm, undecomposable/may-deny) is checked before the allow-list
    /// is even consulted, so approving a command that currently gets any `Ask` or `Deny` from
    /// that analysis would be a no-op. A prefix containing a literal `*` is also refused:
    /// `glob_match` has no escape syntax, so storing it as a glob would turn the literal
    /// character into a wildcard.
    fn remember_bash(&self, command: &str) -> bool {
        if !self.sandbox_available {
            return false;
        }
        let Some(prefixes) = harness_shell::session_prefixes(command) else {
            return false;
        };
        if prefixes.is_empty() || prefixes.iter().any(|p| p.contains('*')) {
            return false;
        }
        match harness_shell::evaluate(command, &self.bash_rules(), &self.workspace) {
            Verdict::Deny { .. } | Verdict::Ask { .. } => return false,
            Verdict::Allow | Verdict::Unlisted => {}
        }
        let rules: Vec<String> = prefixes.iter().map(|p| format!("bash:{p} *")).collect();
        self.session_bash
            .lock()
            .expect("session bash lock")
            .extend(rules);
        true
    }

    /// Remembers a write's resolved path as an exact session approval. Returns `false` for a
    /// write outside the workspace, inside `.git`, or matching a deny/confirm rule — none of
    /// those decisions can be changed by an approval, since they're checked before the session
    /// approvals in `check_write`.
    fn remember_write(&self, path: &Path) -> bool {
        let target = resolve_path(&self.workspace, path);
        let Ok(inside) = target.strip_prefix(&self.workspace) else {
            return false;
        };
        if inside.components().any(|c| is_dot_git(c.as_os_str())) {
            return false;
        }
        if self
            .path_rule(&self.rules.deny, "write", &target, true)
            .is_some()
            || self
                .path_rule(&self.rules.confirm, "write", &target, true)
                .is_some()
        {
            return false;
        }
        self.session_paths
            .lock()
            .expect("session paths lock")
            .insert(("write", target));
        true
    }

    /// Remembers a read's resolved path as an exact session approval. Returns `false` when it
    /// matches a deny/confirm rule, since remembering it couldn't change that decision.
    fn remember_read(&self, path: &Path) -> bool {
        let target = resolve_path(&self.workspace, path);
        if self
            .path_rule(&self.rules.deny, "read", &target, true)
            .is_some()
            || self
                .path_rule(&self.rules.confirm, "read", &target, true)
                .is_some()
        {
            return false;
        }
        self.session_paths
            .lock()
            .expect("session paths lock")
            .insert(("read", target));
        true
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
        match action {
            Action::Bash(command) => self.remember_bash(command),
            Action::Write(path) => self.remember_write(path),
            Action::Read(path) => self.remember_read(path),
        }
    }
}
