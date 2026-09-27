//! git argument parsing: global options, and the destructive subcommands
//! (`push`, `reset`, `clean`, `checkout`, `restore`, `switch`).

use crate::argv::{Tok, command_name};

/// Parsed git global options (`git [global options] <subcommand> …`).
pub(crate) struct GitCall {
    /// Index in argv of the subcommand token.
    pub sub: Option<usize>,
    /// `--config-env`, `--exec-path`, or a `-c` setting that is not [`inert_setting`] was
    /// given: config can run arbitrary programs.
    pub overrides_config: bool,
}

/// Parses the global options of `argv` (whose first element is git).
pub(crate) fn parse(argv: &[Tok]) -> GitCall {
    let mut call = GitCall {
        sub: None,
        overrides_config: false,
    };
    let mut i = 1;
    while let Some(tok) = argv.get(i) {
        let Tok::Lit(s) = tok else {
            call.sub = Some(i);
            break;
        };
        match s.as_str() {
            "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--attr-source" => i += 2,
            "-c" => {
                let setting = argv.get(i + 1).and_then(Tok::lit);
                call.overrides_config |= !setting.is_some_and(inert_setting);
                i += 2;
            }
            "--config-env" => {
                call.overrides_config = true;
                i += 2;
            }
            _ if s.starts_with("--config-env=") || s.starts_with("--exec-path") => {
                call.overrides_config = true;
                i += 1;
            }
            // `--git-dir=x`, `--no-pager`, `-P`, `--bare`, …
            _ if s.starts_with('-') => i += 1,
            _ => {
                call.sub = Some(i);
                break;
            }
        }
    }
    call
}

/// Whether `git -c <setting>` sets only a value git never runs as a program: the
/// identity, the initial branch name, colours, advice, path quoting, or signing turned
/// off. Sections and keys match in any case, as git's do.
fn inert_setting(setting: &str) -> bool {
    let (key, value) = match setting.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (setting, None),
    };
    let key = key.to_ascii_lowercase();
    match key.as_str() {
        "user.name" | "user.email" | "init.defaultbranch" | "core.quotepath" => true,
        "commit.gpgsign" | "tag.gpgsign" => value.is_some_and(|v| v.eq_ignore_ascii_case("false")),
        _ => ["color.", "advice."].iter().any(|section| {
            key.strip_prefix(section)
                .is_some_and(|rest| !rest.is_empty())
        }),
    }
}

/// `git <global options> sub args…` → `git sub args…`, so deny rules like
/// `git push*` see through `git -C dir push`.
pub(crate) fn without_globals(argv: &[Tok]) -> Option<Vec<Tok>> {
    if argv.first()?.lit().map(command_name)? != "git" {
        return None;
    }
    let sub = parse(argv).sub.filter(|&s| s > 1)?;
    let mut form = vec![Tok::Lit("git".into())];
    form.extend_from_slice(&argv[sub..]);
    Some(form)
}

/// Why this git invocation is destructive, or `None`.
pub(crate) fn destructive(argv: &[Tok]) -> Option<String> {
    let sub = parse(argv).sub?;
    let Tok::Lit(name) = &argv[sub] else {
        return Some("runs a git subcommand only known at run time".into());
    };
    let (spec, rule): (&Spec, fn(&GitArgs) -> Option<&'static str>) = match name.as_str() {
        "push" => (&PUSH, push),
        "reset" => (&RESET, reset),
        "clean" => (&CLEAN, clean),
        "checkout" => (&CHECKOUT, checkout),
        "restore" => (&RESTORE, restore),
        "switch" => (&SWITCH, switch),
        _ => return None,
    };
    let args = GitArgs::scan(&argv[sub + 1..], spec);
    // A run-time argument could be any option or pathspec.
    let why = rule(&args).or(args
        .dynamic
        .then_some("has git arguments only known at run time"));
    why.map(String::from)
}

fn push(a: &GitArgs) -> Option<&'static str> {
    let forced = a.short('f')
        || a.maybe("force")
        || a.maybe("force-with-lease")
        || a.maybe("force-if-includes");
    let refspec = a
        .operands
        .iter()
        .chain(&a.after_dashdash)
        .filter_map(|t| t.lit())
        .any(|r| r.starts_with(['+', ':']));
    let deletes = a.short('d') || a.maybe("delete") || a.maybe("mirror") || a.maybe("prune");
    if forced || refspec {
        Some("force-pushes, rewriting remote history")
    } else {
        deletes.then_some("deletes remote refs")
    }
}

fn reset(a: &GitArgs) -> Option<&'static str> {
    a.maybe("hard").then_some("discards uncommitted changes")
}

fn clean(a: &GitArgs) -> Option<&'static str> {
    let dry_run = a.short('n') || a.definitely("dry-run");
    (!dry_run).then_some("deletes untracked files")
}

fn checkout(a: &GitArgs) -> Option<&'static str> {
    let forced = a.short('f') || a.maybe("force");
    // Branch names cannot start with `.`, `:` or `/`, so such operands are pathspecs.
    let pathspec = !a.after_dashdash.is_empty()
        || a.maybe("pathspec-from-file")
        || a.operands.iter().any(|t| match t {
            Tok::Lit(s) => s.starts_with(['.', ':', '/']),
            _ => true,
        });
    (forced || pathspec).then_some("overwrites local changes")
}

fn switch(a: &GitArgs) -> Option<&'static str> {
    let discards = a.short('f') || a.maybe("force") || a.maybe("discard-changes");
    discards.then_some("discards local changes")
}

fn restore(a: &GitArgs) -> Option<&'static str> {
    let staged = a.short('S') || a.definitely("staged");
    let worktree = a.short('W') || a.maybe("worktree");
    (!staged || worktree).then_some("overwrites working-tree changes")
}

/// Option grammar of one subcommand (parse-options style); names are space-separated.
struct Spec {
    long: &'static str,
    /// Long options whose value may be the next argument.
    long_arg: &'static str,
    /// Short options that take a value (rest of the cluster or next argument).
    short_arg: &'static str,
}

const PUSH: Spec = Spec {
    long: "all branches mirror delete tags follow-tags dry-run porcelain force \
           force-with-lease force-if-includes repo set-upstream prune verify no-verify thin \
           quiet verbose progress recurse-submodules atomic push-option receive-pack exec \
           signed ipv4 ipv6",
    long_arg: "repo push-option receive-pack exec",
    short_arg: "o",
};

const RESET: Spec = Spec {
    long: "hard soft mixed merge keep quiet no-quiet patch intent-to-add recurse-submodules \
           no-refresh refresh pathspec-from-file pathspec-file-nul",
    long_arg: "pathspec-from-file",
    short_arg: "",
};

const CLEAN: Spec = Spec {
    long: "dry-run force interactive quiet exclude",
    long_arg: "exclude",
    short_arg: "e",
};

const CHECKOUT: Spec = Spec {
    long: "quiet progress no-progress force ours theirs track no-track guess no-guess detach \
           orphan ignore-skip-worktree-bits merge conflict patch ignore-other-worktrees \
           overwrite-ignore no-overwrite-ignore recurse-submodules no-recurse-submodules \
           overlay no-overlay pathspec-from-file pathspec-file-nul",
    long_arg: "orphan pathspec-from-file",
    short_arg: "bB",
};

const RESTORE: Spec = Spec {
    long: "source staged worktree ignore-unmerged overlay no-overlay quiet progress \
           no-progress ours theirs merge conflict patch ignore-skip-worktree-bits \
           recurse-submodules no-recurse-submodules pathspec-from-file pathspec-file-nul",
    long_arg: "source pathspec-from-file",
    short_arg: "s",
};

const SWITCH: Spec = Spec {
    long: "create force-create guess discard-changes quiet recurse-submodules progress merge \
           conflict detach track force orphan overwrite-ignore ignore-other-worktrees",
    long_arg: "create force-create conflict orphan",
    short_arg: "cC",
};

/// A subcommand's arguments split parse-options style (options may follow operands).
struct GitArgs<'a> {
    shorts: Vec<char>,
    /// For each long option: the known names its (possibly abbreviated) spelling can
    /// denote. Negated `--no-x` spellings are dropped.
    longs: Vec<Vec<&'static str>>,
    operands: Vec<&'a Tok>,
    after_dashdash: Vec<&'a Tok>,
    dynamic: bool,
}

impl<'a> GitArgs<'a> {
    fn scan(args: &'a [Tok], spec: &Spec) -> Self {
        let mut out = GitArgs {
            shorts: Vec::new(),
            longs: Vec::new(),
            operands: Vec::new(),
            after_dashdash: Vec::new(),
            dynamic: false,
        };
        let mut dashdash = false;
        let mut i = 0;
        while let Some(tok) = args.get(i) {
            i += 1;
            let s = match tok {
                Tok::Dyn => {
                    out.dynamic = true;
                    continue;
                }
                _ if dashdash => {
                    out.after_dashdash.push(tok);
                    continue;
                }
                Tok::Glob { .. } => {
                    out.operands.push(tok);
                    continue;
                }
                Tok::Lit(s) => s.as_str(),
            };
            if s == "--" {
                dashdash = true;
            } else if let Some(body) = s.strip_prefix("--") {
                let (name, has_value) = match body.split_once('=') {
                    Some((name, _)) => (name, true),
                    None => (body, false),
                };
                let known = |n: &str| spec.long.split_whitespace().any(|l| l == n);
                if name.starts_with("no-") && !known(name) {
                    continue; // `--no-force` and friends never enable anything
                }
                let names = resolve(name, spec.long);
                if !has_value
                    && names.len() == 1
                    && spec.long_arg.split_whitespace().any(|l| l == names[0])
                {
                    i += 1;
                }
                out.longs.push(names);
            } else if let Some(cluster) = s.strip_prefix('-').filter(|c| !c.is_empty()) {
                for (at, c) in cluster.char_indices() {
                    out.shorts.push(c);
                    if spec.short_arg.contains(c) {
                        // The value is the rest of the cluster, or else the next argument.
                        if at + c.len_utf8() == cluster.len() {
                            i += 1;
                        }
                        break;
                    }
                }
            } else {
                out.operands.push(tok);
            }
        }
        out
    }

    fn short(&self, c: char) -> bool {
        self.shorts.contains(&c)
    }

    /// Some option may be `name` (ambiguous abbreviations count).
    fn maybe(&self, name: &str) -> bool {
        self.longs.iter().any(|names| names.contains(&name))
    }

    /// Some option is certainly `name` (exact or unambiguous abbreviation).
    fn definitely(&self, name: &str) -> bool {
        self.longs.iter().any(|names| names == &[name])
    }
}

/// The long options `name` can denote: an exact match wins, else every option it
/// abbreviates.
fn resolve(name: &str, long: &'static str) -> Vec<&'static str> {
    if let Some(exact) = long.split_whitespace().find(|l| *l == name) {
        return vec![exact];
    }
    if name.is_empty() {
        return Vec::new();
    }
    long.split_whitespace()
        .filter(|l| l.starts_with(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn destructive_cmd(cmd: &str) -> bool {
        let argv: Vec<Tok> = cmd.split(' ').map(|w| Tok::Lit(w.into())).collect();
        destructive(&argv).is_some()
    }

    #[test]
    fn destructive_operations() {
        for cmd in [
            "git push -f",
            "git push -uf origin main",
            "git push --force-with-lease",
            "git push --for",
            "git push origin +main",
            "git push origin :old",
            "git push --delete origin x",
            "git --no-pager -C x push --mirror",
            "git reset --har",
            "git clean -xdf",
            "git clean --forc",
            "git clean -en",
            "git checkout -f main",
            "git checkout -- a.rs",
            "git checkout .",
            "git restore a.rs",
            "git restore --s a.rs",
            "git restore -SW a.rs",
            "git switch -f main",
            "git switch --discard main",
            "git switch -c x --for",
            "git switch -Cnew -f",
        ] {
            assert!(destructive_cmd(cmd), "{cmd}");
        }
    }

    #[test]
    fn safe_operations() {
        for cmd in [
            "git push origin main",
            "git push --no-force origin main",
            "git push -o ci.skip origin main",
            "git reset --soft HEAD",
            "git clean -n",
            "git clean --dry",
            "git checkout -b feature",
            "git checkout main",
            "git restore --staged a.rs",
            "git restore --st a.rs",
            "git switch main",
            "git switch -c -f",
            "git switch --conflict diff3 -m main",
            "git switch --no-force main",
            "git status",
        ] {
            assert!(!destructive_cmd(cmd), "{cmd}");
        }
    }

    #[test]
    fn global_options() {
        let argv: Vec<Tok> = "git -c a=b --git-dir x --no-pager log"
            .split(' ')
            .map(|w| Tok::Lit(w.into()))
            .collect();
        let call = parse(&argv);
        assert_eq!(call.sub, Some(6));
        assert!(call.overrides_config);
        assert_eq!(without_globals(&argv).unwrap().len(), 2);
    }

    #[test]
    fn inert_settings() {
        for setting in [
            "user.name=x",
            "USER.Email=a@b",
            "init.defaultBranch=main",
            "color.ui",
            "color.diff.meta=blue",
            "Advice.detachedHead=false",
            "core.quotePath=off",
            "commit.gpgsign=false",
            "tag.gpgSign=FALSE",
        ] {
            assert!(inert_setting(setting), "{setting}");
        }
        for setting in [
            "core.pager=less",
            "commit.gpgsign",
            "commit.gpgsign=no",
            "color",
            "color.=x",
            "colors.ui=1",
            "user.namex=y",
            "user.name.x=y",
            "alias.st=!sh",
        ] {
            assert!(!inert_setting(setting), "{setting}");
        }
    }
}
