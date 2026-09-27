//! Decides whether each action may run, needs approval, or is refused, from the approval mode, the
//! user's rules, the shell-command analysis, path boundaries, and whether an OS sandbox exists.

use std::{
    collections::HashSet,
    ffi::OsStr,
    io::Read,
    path::{Component, Path, PathBuf},
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
    /// Raw config rules, exactly as given: used only for bash filtering (`bash_rules`) and
    /// `unknown_rules`. Read/write matching uses the expanded `*_paths` below instead.
    rules: RuleSet,
    /// Config `read:`/`write:` allow rules, `~`-expanded. Deliberately has no symlink-resolved
    /// twin: an allow rule must never be widened by a symlink someone plants under a path it
    /// names (see `path_rules`).
    allow_paths: Vec<PathRule>,
    /// Config `read:`/`write:` deny rules, `~`-expanded, each with a symlink-resolved twin glob
    /// when the configured glob is absolute.
    deny_paths: Vec<PathRule>,
    /// Config `read:`/`write:` confirm rules, expanded the same way as `deny_paths`.
    confirm_paths: Vec<PathRule>,
    sandbox_available: bool,
    /// Where `<workspace>/.git` sends git when it is a symlink or a `gitdir:` file, resolved.
    /// Writes under it are guarded like writes under `.git`.
    linked_gitdir: Option<PathBuf>,
    /// Bash allow-glob prefixes added by approve-for-session.
    session_bash: Mutex<Vec<String>>,
    /// Exact (tool, resolved path) approvals added by approve-for-session. Stored as exact
    /// paths rather than globs, so a literal `*` in an approved path is never treated as a
    /// wildcard over its siblings (a `glob_match` pattern has no escape syntax).
    session_paths: Mutex<HashSet<(&'static str, PathBuf)>>,
}

/// A single `read:`/`write:` rule, expanded for matching. `display` is exactly what the user
/// configured (after `~` expansion) — used in every message, so a denial always names the rule
/// as written, never an internal resolved form. `globs` holds the glob(s) actually compared
/// against a candidate path: just `display`, or `display` plus its symlink-resolved twin.
struct PathRule {
    tool: &'static str,
    display: String,
    globs: Vec<String>,
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

/// Whether `component` names `name` on a case-insensitive filesystem (macOS's default), where
/// `.GIT` reaches `.git`. Folds beyond ASCII too, as APFS does: `ſ` matches `s` and the Kelvin
/// sign matches `k`.
fn same_name(component: &OsStr, name: &str) -> bool {
    fn fold(s: &str) -> String {
        s.chars()
            .flat_map(char::to_uppercase)
            .flat_map(char::to_lowercase)
            .collect()
    }
    component.to_str().is_some_and(|c| fold(c) == fold(name))
}

fn is_dot_git(component: &OsStr) -> bool {
    same_name(component, ".git")
}

/// `path` relative to `base`, comparing each component case-insensitively. `None` if `path`
/// isn't under `base` this way.
fn strip_prefix_ci(path: &Path, base: &Path) -> Option<PathBuf> {
    let mut remaining = path.components();
    for base_component in base.components() {
        let component = remaining.next()?;
        let base_text = base_component.as_os_str().to_string_lossy().to_lowercase();
        let text = component.as_os_str().to_string_lossy().to_lowercase();
        if base_text != text {
            return None;
        }
    }
    Some(remaining.as_path().to_path_buf())
}

/// The gitdir `<workspace>/.git` points git to when it is a symlink or a `gitdir:` file, resolved
/// (it may not exist yet). `None` when `.git` is missing, a plain directory, or a file git would
/// not accept.
fn linked_gitdir(workspace: &Path) -> Option<PathBuf> {
    let dot_git = workspace.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    let target = if meta.file_type().is_symlink() {
        let resolved = resolve_path(workspace, Path::new(".git"));
        if !std::fs::metadata(&resolved).is_ok_and(|m| m.is_file()) {
            return Some(resolved);
        }
        resolved
    } else if meta.is_file() {
        dot_git
    } else {
        return None;
    };
    let mut text = String::new();
    std::fs::File::open(&target)
        .ok()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    let gitdir = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    // Git resolves a relative gitdir against the directory holding `.git`.
    (!gitdir.is_empty()).then(|| resolve_path(workspace, Path::new(gitdir)))
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

/// Builds the `read:`/`write:` `PathRule`s out of a config rule list: expands a leading `~/`
/// using `$HOME`, then — only when `twin_symlinks` is set — adds a symlink-resolved twin glob
/// for any resulting absolute glob, so e.g. a rule on `/etc/*` still matches macOS's real
/// `/private/etc/*`, and `~/.ssh/*` matches the user's actual home directory. `twin_symlinks`
/// must be `false` for allow rules: an allow rule must match only the path exactly as
/// configured, never widen its reach to wherever a symlink under that path happens to point
/// (deny/confirm rules are safe to widen this way, since widening a refusal is conservative).
/// Rules for tools other than `read`/`write` are dropped (bash filtering uses the raw config
/// list directly; see `bash_rules`).
fn path_rules(rules: &[String], home: Option<&Path>, twin_symlinks: bool) -> Vec<PathRule> {
    let mut out = Vec::new();
    for rule in rules {
        let Some((tool, glob)) = rule.split_once(':') else {
            continue;
        };
        let tool: &'static str = match tool {
            "read" => "read",
            "write" => "write",
            _ => continue,
        };
        let expanded = match (glob.strip_prefix("~/"), home) {
            (Some(rest), Some(home)) => format!("{}/{rest}", home.display()),
            _ => glob.to_string(),
        };
        let mut globs = vec![expanded.clone()];
        if twin_symlinks
            && expanded.starts_with('/')
            && let Some(resolved) = resolve_glob_prefix(&expanded)
        {
            globs.push(resolved);
        }
        out.push(PathRule {
            tool,
            display: expanded,
            globs,
        });
    }
    out
}

/// `path` joined to `workspace` if relative, normalized for `.` and `..` components, but with
/// no symlink ever followed — unlike `resolve_path`. This lets a deny/confirm rule match a
/// symlink by its own leaf name, even though the target it points to (what `resolve_path`
/// would give) is a different path entirely.
fn lexical_path(workspace: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
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
        let allow_paths = path_rules(&config.rules.allow, home.as_deref(), false);
        let deny_paths = path_rules(&config.rules.deny, home.as_deref(), true);
        let confirm_paths = path_rules(&config.rules.confirm, home.as_deref(), true);
        let workspace = resolved(&config.workspace);
        PermissionEngine {
            mode: config.mode,
            linked_gitdir: linked_gitdir(&workspace),
            workspace,
            read_dirs: config.read_dirs.iter().map(|d| resolved(d)).collect(),
            rules: config.rules,
            allow_paths,
            deny_paths,
            confirm_paths,
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

    /// First allow rule for `tool` matching `target` (the fully resolved path). Case-sensitive,
    /// and only ever matches the resolved target — never a symlink-resolved twin, and never a
    /// lexical (symlink-unaware) form — so an allow rule can never be widened by a symlink
    /// planted under a path it names.
    fn allow_rule(&self, list: &[PathRule], tool: &str, target: &Path) -> Option<String> {
        let absolute = target.display().to_string();
        let relative = target
            .strip_prefix(&self.workspace)
            .ok()
            .map(|r| r.display().to_string());
        list.iter()
            .filter(|rule| rule.tool == tool)
            .find(|rule| {
                rule.globs.iter().any(|glob| {
                    harness_shell::glob_match(glob, &absolute)
                        || relative
                            .as_deref()
                            .is_some_and(|r| harness_shell::glob_match(glob, r))
                })
            })
            .map(|rule| format!("{tool}:{}", rule.display))
    }

    /// First deny/confirm rule for `tool` matching `target` (the fully resolved path) or
    /// `lexical` (the symlink-unaware, `.`/`..`-normalized path) — as absolute text or
    /// workspace-relative text, matched case-insensitively. Checking both `target` and
    /// `lexical` means a rule matches whichever a symlink involves: naming what it points to
    /// (`target`), or naming its own leaf (`lexical`). Returns the rule as configured
    /// (`tool:display`), never a resolved twin, so messages always name the rule as written.
    fn deny_confirm_rule(
        &self,
        list: &[PathRule],
        tool: &str,
        target: &Path,
        lexical: &Path,
    ) -> Option<String> {
        let candidates: Vec<String> = [target, lexical]
            .into_iter()
            .flat_map(|p| {
                let absolute = p.display().to_string().to_lowercase();
                let relative = self
                    .relative_ci(p)
                    .map(|r| r.display().to_string().to_lowercase());
                std::iter::once(absolute).chain(relative)
            })
            .collect();
        list.iter()
            .filter(|rule| rule.tool == tool)
            .find(|rule| {
                rule.globs.iter().any(|glob| {
                    let glob = glob.to_lowercase();
                    candidates
                        .iter()
                        .any(|c| harness_shell::glob_match(&glob, c))
                })
            })
            .map(|rule| format!("{tool}:{}", rule.display))
    }

    /// `path` relative to the workspace, comparing each component case-insensitively (so a
    /// target spelled with a different case than the configured workspace still strips).
    /// `None` if `path` isn't under the workspace this way.
    fn relative_ci(&self, path: &Path) -> Option<PathBuf> {
        strip_prefix_ci(path, &self.workspace)
    }

    /// Why a write inside the workspace needs approval whatever the mode's defaults: it lands in
    /// git metadata (hooks and config can run commands) or in `.harness/`, or creates a top-level
    /// `HEAD`, which would make git take the workspace for a repository. The same set the sandbox
    /// protects. `target` is the resolved path and `lexical` the symlink-unaware one; either
    /// matching is enough.
    fn protected_write(&self, target: &Path, lexical: &Path) -> Option<&'static str> {
        let inside: Vec<PathBuf> = [target, lexical]
            .into_iter()
            .filter_map(|p| self.relative_ci(p))
            .collect();
        let first_is = |name: &str| {
            inside.iter().any(|r| {
                r.components()
                    .next()
                    .is_some_and(|c| same_name(c.as_os_str(), name))
            })
        };
        if inside
            .iter()
            .any(|r| r.components().any(|c| is_dot_git(c.as_os_str())))
            || self
                .linked_gitdir
                .as_deref()
                .is_some_and(|gitdir| strip_prefix_ci(target, gitdir).is_some())
        {
            Some("write inside .git (hooks and config can run commands)")
        } else if first_is(".harness") {
            Some("write inside .harness (harness's project settings)")
        } else if first_is("HEAD") {
            Some("write a top-level HEAD (git would take the workspace for a repository)")
        } else {
            None
        }
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
        let lexical = lexical_path(&self.workspace, path);
        if let Some(rule) = self.deny_confirm_rule(&self.deny_paths, "read", &target, &lexical) {
            return Decision::Deny(format!("denied by rule `{rule}`"));
        }
        if self.mode != Mode::FullAccess
            && let Some(rule) =
                self.deny_confirm_rule(&self.confirm_paths, "read", &target, &lexical)
        {
            return Decision::Ask(format!("read {} (confirm rule `{rule}`)", target.display()));
        }
        if self.mode == Mode::FullAccess
            || target.starts_with(&self.workspace)
            || self.read_dirs.iter().any(|d| target.starts_with(d))
            || self
                .allow_rule(&self.allow_paths, "read", &target)
                .is_some()
            || self.session_path_allowed("read", &target)
        {
            return Decision::Allow;
        }
        Decision::Ask(format!("read outside the workspace: {}", target.display()))
    }

    fn check_write(&self, path: &Path) -> Decision {
        let target = resolve_path(&self.workspace, path);
        let lexical = lexical_path(&self.workspace, path);
        if let Some(rule) = self.deny_confirm_rule(&self.deny_paths, "write", &target, &lexical) {
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
        if let Some(reason) = self.protected_write(&target, &lexical) {
            return Decision::Ask(reason.into());
        }
        if let Some(rule) = self.deny_confirm_rule(&self.confirm_paths, "write", &target, &lexical)
        {
            return Decision::Ask(format!(
                "write {} (confirm rule `{rule}`)",
                inside.display()
            ));
        }
        if self.mode == Mode::Auto
            || self
                .allow_rule(&self.allow_paths, "write", &target)
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
            _ if !self.sandbox_available && matches!(self.mode, Mode::Plan | Mode::ReadOnly) => {
                Decision::Deny(
                    "shell commands need the OS sandbox in plan and read-only mode".into(),
                )
            }
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

    /// Remembers a write's resolved path as an exact session approval. Returns `false` in plan
    /// or read-only mode (writes are refused outright, so an approval changes nothing), for a
    /// write outside the workspace, to a protected path (`protected_write`), or matching a
    /// deny/confirm rule — none of those decisions can be changed by an approval, since they're
    /// checked before the session approvals in `check_write`.
    fn remember_write(&self, path: &Path) -> bool {
        if matches!(self.mode, Mode::Plan | Mode::ReadOnly) {
            return false;
        }
        let target = resolve_path(&self.workspace, path);
        let lexical = lexical_path(&self.workspace, path);
        if target.strip_prefix(&self.workspace).is_err()
            || self.protected_write(&target, &lexical).is_some()
        {
            return false;
        }
        if self
            .deny_confirm_rule(&self.deny_paths, "write", &target, &lexical)
            .is_some()
            || self
                .deny_confirm_rule(&self.confirm_paths, "write", &target, &lexical)
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
        let lexical = lexical_path(&self.workspace, path);
        if self
            .deny_confirm_rule(&self.deny_paths, "read", &target, &lexical)
            .is_some()
            || self
                .deny_confirm_rule(&self.confirm_paths, "read", &target, &lexical)
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
