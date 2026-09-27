//! Rule evaluation: turns the analysis of a command line into a [`Verdict`].

use std::path::Path;

use crate::argv::{Tok, command_name, display, quote};
use crate::matching::{argv_matches, argv_matches_any_case, argv_may_match};
use crate::parse::analyze;
use crate::paths::Workspace;

/// User rules for the bash tool; patterns are written without the `bash:` prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub confirm: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every sub-command decomposed and matched an allow rule.
    Allow,
    /// Fully decomposed, nothing denied/destructive/confirm-matched, but not every
    /// sub-command is allow-listed. (In the harness's `auto` mode this runs sandboxed
    /// without a prompt; in `ask` mode it prompts.)
    Unlisted,
    /// Must prompt regardless of mode: undecomposable input, destructive command, or
    /// a `confirm` rule match.
    Ask {
        reason: String,
        destructive: bool,
        /// Whether a `deny` rule could apply here even though it didn't definitely match: a
        /// possible deny match, or an undecomposable command while any `deny` rule exists (it
        /// could be hiding a denied one). Callers that let destructive/confirm-only asks
        /// through in a fully trusted mode must still refuse when this is set, since deny
        /// rules are meant to win in every mode.
        may_deny: bool,
    },
    /// A deny rule matched (deny still wins over everything).
    Deny { reason: String },
}

/// Decides how `command` may run. `workspace` is the project root; the command is
/// assumed to start in it.
///
/// Deny and confirm rules match in any case; allow rules only as written.
///
/// Order: definite deny ⇒ `Deny`; may-match deny (including a deny match in possible
/// here-document text) / undecomposable / destructive ⇒ one `Ask` carrying every reason
/// (destructive flag set when anything destructive was found);
/// definite or may-match confirm, sudo-like wrappers, writes outside the workspace ⇒ `Ask`;
/// all allow-listed ⇒ `Allow`; otherwise `Unlisted`.
pub fn evaluate(command: &str, rules: &Rules, workspace: &Path) -> Verdict {
    let a = analyze(command, &Workspace::new(workspace));
    if let Some((form, rule)) = first_match(&a.forms, &rules.deny) {
        return Verdict::Deny {
            reason: format!("`{}` matches deny rule `bash:{rule}`", display(form)),
        };
    }
    let destructive = !a.destructive.is_empty();
    let mut reasons = Vec::new();
    let mut may_deny = false;
    if let Some((form, rule)) = first_possible_match(&a.forms, &rules.deny) {
        reasons.push(format!(
            "`{}` may match deny rule `bash:{rule}` (part of it is only known at run time)",
            display(form)
        ));
        may_deny = true;
    } else if let Some((form, rule)) = first_match(&a.data_forms, &rules.deny)
        .or_else(|| first_possible_match(&a.data_forms, &rules.deny))
    {
        reasons.push(format!(
            "`{}` may match deny rule `bash:{rule}` (it may be here-document text)",
            display(form)
        ));
        may_deny = true;
    }
    if !a.undecomposable.is_empty() {
        reasons.push(format!(
            "cannot fully analyze the command: {}",
            a.undecomposable.join("; ")
        ));
        // An undecomposable command could be hiding a denied one.
        may_deny = may_deny || !rules.deny.is_empty();
    }
    if destructive {
        reasons.push(format!("destructive: {}", a.destructive.join("; ")));
    }
    if !reasons.is_empty() {
        return Verdict::Ask {
            reason: reasons.join("; "),
            destructive,
            may_deny,
        };
    }
    if let Some((form, rule)) = first_match(&a.forms, &rules.confirm) {
        return Verdict::Ask {
            reason: format!("`{}` matches confirm rule `bash:{rule}`", display(form)),
            destructive: false,
            may_deny: false,
        };
    }
    if let Some((form, rule)) = first_possible_match(&a.forms, &rules.confirm) {
        return Verdict::Ask {
            reason: format!(
                "`{}` may match confirm rule `bash:{rule}` (part of it is only known at run time)",
                display(form)
            ),
            destructive: false,
            may_deny: false,
        };
    }
    if !a.ask.is_empty() {
        return Verdict::Ask {
            reason: a.ask.join("; "),
            destructive: false,
            may_deny: false,
        };
    }
    let listed = !a.commands.is_empty()
        && a.unlisted.is_empty()
        && a.commands
            .iter()
            .all(|argv| rules.allow.iter().any(|p| argv_matches(p, argv)));
    if listed {
        Verdict::Allow
    } else {
        Verdict::Unlisted
    }
}

/// The first deny or confirm rule matching a form, ignoring case.
fn first_match<'a>(forms: &'a [Vec<Tok>], patterns: &'a [String]) -> Option<(&'a [Tok], &'a str)> {
    forms.iter().find_map(|form| {
        let rule = patterns.iter().find(|p| argv_matches_any_case(p, form))?;
        Some((form.as_slice(), rule.as_str()))
    })
}

fn first_possible_match<'a>(
    forms: &'a [Vec<Tok>],
    patterns: &'a [String],
) -> Option<(&'a [Tok], &'a str)> {
    forms.iter().find_map(|form| {
        let rule = patterns.iter().find(|p| argv_may_match(p, form))?;
        Some((form.as_slice(), rule.as_str()))
    })
}

/// Tools whose first argument selects a subcommand, so `cargo test` rather than
/// `cargo` is the unit a user approves.
const SUBCOMMAND_TOOLS: &[&str] = &[
    "cargo", "git", "npm", "pnpm", "yarn", "bun", "npx", "go", "docker", "kubectl", "make", "just",
    "gh", "uv",
];

/// `session_prefixes` has no workspace: relative paths are resolved inside this
/// placeholder root, so only they can count as non-destructive.
const DETACHED_WORKSPACE: &str = "/.harness-session-workspace";

/// Literal prefixes to allow when the user approves `command` for the session, one
/// per distinct sub-command. `None` when that would be unsafe or ineffective: the
/// command is undecomposable, destructive, needs a prompt anyway (`sudo`, writes
/// outside the workspace), or cannot be allow-listed as written (assignments,
/// redirects to files, run-time command names, …).
///
/// A prefix is `argv0 subcommand` for [`SUBCOMMAND_TOOLS`] and plain `argv0`
/// otherwise (`rm -rf x` → `rm`; destructive uses still prompt). If such a tool's
/// first argument is an option (`cargo +nightly test`), the whole literal command
/// line is the prefix.
pub fn session_prefixes(command: &str) -> Option<Vec<String>> {
    let a = analyze(command, &Workspace::new(Path::new(DETACHED_WORKSPACE)));
    let clean = a.undecomposable.is_empty()
        && a.destructive.is_empty()
        && a.ask.is_empty()
        && a.unlisted.is_empty();
    if !clean || a.commands.is_empty() {
        return None;
    }
    let mut prefixes: Vec<String> = Vec::new();
    for argv in &a.commands {
        let p = prefix(argv)?;
        if !prefixes.contains(&p) {
            prefixes.push(p);
        }
    }
    Some(prefixes)
}

fn prefix(argv: &[Tok]) -> Option<String> {
    let name = argv.first()?.lit()?;
    if !SUBCOMMAND_TOOLS.contains(&command_name(name).as_str()) {
        return Some(quote(name).into_owned());
    }
    match argv.get(1) {
        None => Some(quote(name).into_owned()),
        Some(Tok::Lit(sub)) if !sub.starts_with(['-', '+']) => {
            Some(format!("{} {}", quote(name), quote(sub)))
        }
        _ => {
            let words: Option<Vec<_>> = argv.iter().map(|t| t.lit().map(quote)).collect();
            Some(words?.join(" "))
        }
    }
}
