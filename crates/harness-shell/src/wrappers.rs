//! Commands that run other commands (`env`, `sudo`, `bash -c`, `xargs`, …).
//!
//! Option tables are best effort. When the command word directly follows an option
//! (`sudo --flag word cmd`), that option might have taken the word as its value, so
//! the argv one word further on is also scanned for deny/destructive matches.

use crate::argv::{Tok, basename, command_name};

/// Environment variables whose value makes common tools run another program.
pub(crate) fn runs_programs(var: &str) -> bool {
    var.starts_with("GIT_CONFIG_")
        || matches!(
            var,
            "GIT_EXEC_PATH"
                | "GIT_PAGER"
                | "GIT_SSH"
                | "GIT_SSH_COMMAND"
                | "GIT_EDITOR"
                | "GIT_SEQUENCE_EDITOR"
                | "GIT_EXTERNAL_DIFF"
                | "GIT_ASKPASS"
                | "GIT_PROXY_COMMAND"
                | "PAGER"
                | "EDITOR"
                | "VISUAL"
        )
}

/// The variable a `NAME=value` word sets: `A` for `A+=x` and `A[i]=x` (and for a bare `A`).
pub(crate) fn assigned_name(word: &str) -> &str {
    let before_eq = word.split('=').next().unwrap_or(word);
    let before_bracket = before_eq.split('[').next().unwrap_or(before_eq);
    before_bracket.strip_suffix('+').unwrap_or(before_bracket)
}

/// What a wrapper runs.
pub(crate) enum Next {
    /// An argv, analyzed like any simple command.
    Argv(Vec<Tok>),
    /// Shell source, parsed recursively.
    Script(String),
}

/// The effect of one wrapper layer.
#[derive(Default)]
pub(crate) struct Unwrapped {
    pub next: Vec<Next>,
    /// Other possible readings of the inner argv, scanned for deny/destructive only.
    pub alternatives: Vec<Vec<Tok>>,
    /// Prevents auto-allow (the command stays "unlisted").
    pub unlisted: Option<String>,
    /// Forces a prompt even in modes that run unlisted commands.
    pub ask: Option<String>,
    /// The layer cannot be analyzed at all.
    pub opaque: Option<String>,
    /// The inner command runs in a directory we cannot know.
    pub unknown_cwd: bool,
    /// The inner command runs in the current shell (so `cd` still counts).
    pub same_shell: bool,
}

/// Returns the wrapper's effect, or `None` if `argv` is not a wrapper invocation
/// (it is then the command itself). The name is matched in any case, as a case-insensitive
/// file system finds `SUDO` as `sudo`.
pub(crate) fn unwrap(argv: &[Tok]) -> Option<Unwrapped> {
    let word = basename(argv.first()?.lit()?);
    let name = command_name(word);
    let u = unwrap_named(&name, &argv[1..])?;
    if word == name {
        return Some(u);
    }
    // Spelled another way, the name is never a shell builtin but a program found on the
    // PATH (`/usr/bin/command` runs in a child process), and allow rules do not match it.
    let why = format!("`{word}` is written in a different case from `{name}`");
    Some(Unwrapped {
        same_shell: false,
        unlisted: u.unlisted.or(Some(why)),
        ..u
    })
}

fn unwrap_named(name: &str, args: &[Tok]) -> Option<Unwrapped> {
    let skip = |short_arg, long_arg| scan_options(args, short_arg, long_arg).0;
    match name {
        "command" => command(args),
        "builtin" => inner(args, 0).map(same_shell),
        "exec" => inner(args, skip("a", "")),
        "nohup" => inner(args, skip("", "")),
        "setsid" => {
            inner(args, skip("", "")).map(|u| unlisted(u, "`setsid` runs a detached command"))
        }
        "time" => time(args),
        "nice" => inner(args, skip("n", "adjustment")),
        "timeout" => inner(args, skip("sk", "signal kill-after") + 1),
        "stdbuf" => inner(args, skip("ioe", "input output error")),
        "env" => env(args),
        "sudo" | "doas" | "run0" => Some(privileged(name, args)),
        "sh" | "bash" | "dash" | "zsh" | "ksh" => shell(args),
        "eval" => eval(args),
        "source" | "." => Some(opaque("`source` runs commands from a file")),
        "xargs" => Some(xargs(args)),
        "find" => find(args),
        "parallel" => Some(parallel(args)),
        "watch" => watch(args),
        "flock" => flock(args).map(|u| unlisted(u, "`flock` runs a command under a lock")),
        "ssh" => Some(ssh(args)),
        "trap" => trap(args),
        "compgen" | "complete" => completion_action(name, args),
        _ => None,
    }
}

/// The inner argv starting at `args[start]`.
fn inner(args: &[Tok], start: usize) -> Option<Unwrapped> {
    let rest = args.get(start..).filter(|r| !r.is_empty())?;
    let mut u = Unwrapped {
        next: vec![Next::Argv(rest.to_vec())],
        ..Default::default()
    };
    // An option we do not know may have taken the command word as its value.
    let after_option = start
        .checked_sub(1)
        .and_then(|i| args[i].lit())
        .is_some_and(|s| s.starts_with('-') && s != "--");
    if after_option && rest.len() > 1 {
        u.alternatives.push(rest[1..].to_vec());
    }
    Some(u)
}

fn same_shell(u: Unwrapped) -> Unwrapped {
    Unwrapped {
        same_shell: true,
        ..u
    }
}

fn unlisted(u: Unwrapped, why: &str) -> Unwrapped {
    Unwrapped {
        unlisted: Some(why.into()),
        ..u
    }
}

fn opaque(why: &str) -> Unwrapped {
    Unwrapped {
        opaque: Some(why.into()),
        ..Default::default()
    }
}

/// Joins literal words the way `eval`/`ssh`/`watch` hand them to a shell.
fn join_literal(words: &[Tok]) -> Option<String> {
    let parts: Option<Vec<&str>> = words.iter().map(Tok::lit).collect();
    Some(parts?.join(" "))
}

/// Skips leading options (getopt style, stopping at the first operand or `--`).
/// `short_arg` lists short options taking a value; `long_arg` is a space-separated
/// list of long options taking a value. Returns the index of the first operand and
/// the option names seen.
fn scan_options(args: &[Tok], short_arg: &str, long_arg: &str) -> (usize, Vec<String>) {
    let mut names = Vec::new();
    let mut i = 0;
    while let Some(Tok::Lit(s)) = args.get(i) {
        if s == "--" {
            i += 1;
            break;
        }
        if let Some(body) = s.strip_prefix("--") {
            let name = body.split('=').next().unwrap_or_default();
            let takes_value = long_arg.split_whitespace().any(|l| l == name);
            i += 1 + usize::from(takes_value && !body.contains('='));
            names.push(name.to_string());
        } else if let Some(cluster) = s.strip_prefix('-').filter(|c| !c.is_empty()) {
            i += 1;
            for (at, c) in cluster.char_indices() {
                names.push(c.to_string());
                if short_arg.contains(c) {
                    // The value is the rest of the cluster, or else the next argument.
                    i += usize::from(at + c.len_utf8() == cluster.len());
                    break;
                }
            }
        } else {
            break;
        }
    }
    (i.min(args.len()), names)
}

fn has(names: &[String], wanted: &[&str]) -> bool {
    names.iter().any(|n| wanted.contains(&n.as_str()))
}

fn command(args: &[Tok]) -> Option<Unwrapped> {
    let (start, names) = scan_options(args, "", "");
    // `command -v x` / `-V` only looks the name up.
    if has(&names, &["v", "V"]) {
        return None;
    }
    inner(args, start).map(same_shell)
}

fn time(args: &[Tok]) -> Option<Unwrapped> {
    let (start, names) = scan_options(args, "fo", "format output");
    let mut u = inner(args, start)?;
    if names.iter().any(|n| n != "p" && n != "portability") {
        u.unlisted = Some("`time` options can write files".into());
    }
    Some(u)
}

fn env(args: &[Tok]) -> Option<Unwrapped> {
    let (mut start, names) = scan_options(args, "uCS", "unset chdir split-string");
    if has(&names, &["S", "split-string"]) {
        return Some(opaque("`env -S` splits a string into a command line"));
    }
    // A lone `-` means `-i`.
    if args.get(start).and_then(Tok::lit) == Some("-") {
        start += 1;
    }
    let mut assigns = false;
    let mut dangerous_env = false;
    while let Some(Tok::Lit(s)) = args.get(start) {
        if is_assignment(s) {
            assigns = true;
            if runs_programs(assigned_name(s)) {
                dangerous_env = true;
            }
            start += 1;
        } else {
            break;
        }
    }
    let mut u = inner(args, start)?;
    if dangerous_env {
        u.opaque = Some(
            "`env` sets an environment variable that makes programs run other commands".into(),
        );
    } else if assigns {
        u.unlisted = Some("`env` sets environment variables".into());
    }
    if has(&names, &["C", "chdir"]) {
        u.unknown_cwd = true;
        u.unlisted = Some("`env -C` changes the working directory".into());
    }
    Some(u)
}

fn is_assignment(word: &str) -> bool {
    word.contains('=') && !word.starts_with('=')
}

fn privileged(name: &str, args: &[Tok]) -> Unwrapped {
    let (short_arg, long_arg) = match name {
        "sudo" => (
            "CDgpRrTtUuh",
            "close-from chdir group prompt chroot role command-timeout type other-user user host",
        ),
        "doas" => ("uC", ""),
        _ => (
            "ugD",
            "user group chdir setenv unit property description slice nice",
        ),
    };
    let (mut start, names) = scan_options(args, short_arg, long_arg);
    while args
        .get(start)
        .and_then(Tok::lit)
        .is_some_and(is_assignment)
    {
        start += 1;
    }
    // `sudo -e` edits files instead of running a command.
    let edits = name == "sudo" && has(&names, &["e", "edit"]);
    let next = if edits { None } else { inner(args, start) };
    Unwrapped {
        ask: Some(format!(
            "`{name}` runs the command with elevated privileges"
        )),
        ..next.unwrap_or_default()
    }
}

fn shell(args: &[Tok]) -> Option<Unwrapped> {
    let mut has_c = false;
    let mut stdin = false;
    // `-O`/`+O` sets a `shopt` option, such as extglob, which changes how bash parses.
    let mut shopt = false;
    let mut i = 0;
    while let Some(Tok::Lit(s)) = args.get(i) {
        if s == "--" || s == "-" {
            i += 1;
            break;
        }
        if s.len() < 2 || !s.starts_with(['-', '+']) {
            break;
        }
        i += 1;
        if s == "--rcfile" || s == "--init-file" {
            i += 1;
        } else if !s.starts_with("--") {
            has_c |= s.starts_with('-') && s.contains('c');
            stdin |= s.starts_with('-') && s.contains('s');
            shopt |= s.contains('O');
            // `-o name` / `-O shopt` take the next word.
            i += usize::from(s.contains(['o', 'O']));
        }
    }
    match (has_c, args.get(i)) {
        (true, Some(Tok::Lit(src))) => Some(Unwrapped {
            next: vec![Next::Script(src.clone())],
            opaque: shopt.then(|| {
                "the shell is started with a `shopt` option, which can change how it reads the command"
                    .into()
            }),
            ..Default::default()
        }),
        (true, Some(_)) => Some(opaque(
            "shell -c with a command string only known at run time",
        )),
        (true, None) => None,
        // `bash script.sh` runs a file: treated as an ordinary command.
        (false, Some(_)) if !stdin => None,
        (false, _) => Some(opaque("shell reading commands from stdin")),
    }
}

fn eval(args: &[Tok]) -> Option<Unwrapped> {
    if args.is_empty() {
        return None;
    }
    Some(match join_literal(args) {
        Some(src) => Unwrapped {
            next: vec![Next::Script(src)],
            same_shell: true,
            ..Default::default()
        },
        None => opaque("`eval` of text only known at run time"),
    })
}

/// `trap arg sigspec...` sets `arg` as the action that runs, in the same shell, when a
/// listed signal is caught later. `trap -p`, `trap -l`, `trap - sig` (reset to default) and
/// `trap '' sig` (ignore) run nothing and are left as ordinary commands.
fn trap(args: &[Tok]) -> Option<Unwrapped> {
    match args.first()? {
        Tok::Lit(s) if matches!(s.as_str(), "-p" | "-l" | "-" | "") => None,
        Tok::Lit(src) => Some(Unwrapped {
            next: vec![Next::Script(src.clone())],
            unknown_cwd: true,
            ..Default::default()
        }),
        _ => Some(opaque(
            "`trap` sets an action whose text is only known at run time",
        )),
    }
}

/// `compgen`/`complete`'s `-C command` runs `command` as a full command; `-W wordlist`
/// expands each word of `wordlist` (so a command substitution in it runs); both are
/// analyzed as nested shell text when literal. `-F function` runs a shell function this
/// analysis cannot see, so it always asks. Other options take no action text.
fn completion_action(name: &str, args: &[Tok]) -> Option<Unwrapped> {
    let mut i = 0;
    while let Some(tok) = args.get(i) {
        i += 1;
        let Some(s) = tok.lit() else { continue };
        if s == "--" {
            break;
        }
        let Some(rest) = s.strip_prefix('-').filter(|r| !r.is_empty()) else {
            continue;
        };
        let mut chars = rest.chars();
        let mut opt = None;
        for c in chars.by_ref() {
            if "ACWFGXPSo".contains(c) {
                opt = Some(c);
                break;
            }
        }
        let Some(opt) = opt else { continue };
        let attached = chars.as_str();
        let value = if !attached.is_empty() {
            Some(Tok::Lit(attached.to_owned()))
        } else {
            let v = args.get(i).cloned();
            i += 1;
            v
        };
        if opt == 'F' {
            return Some(opaque(&format!(
                "`{name} -F` runs a shell function whose body this analysis does not see"
            )));
        }
        if opt == 'C' || opt == 'W' {
            return Some(match value {
                Some(Tok::Lit(src)) => Unwrapped {
                    next: vec![Next::Script(src)],
                    unknown_cwd: true,
                    ..Default::default()
                },
                _ => opaque(&format!("`{name} -{opt}` runs text only known at run time")),
            });
        }
    }
    None
}

fn xargs(args: &[Tok]) -> Unwrapped {
    let long_arg = "arg-file delimiter max-args max-procs max-chars process-slot-var";
    let start = scan_options(args, "aEdILnPs", long_arg).0;
    let mut cmd = match &args[start..] {
        [] => vec![Tok::Lit("echo".into())],
        rest => rest.to_vec(),
    };
    // Arguments read from stdin are appended at run time.
    cmd.push(Tok::Dyn);
    Unwrapped {
        next: vec![Next::Argv(cmd)],
        unlisted: Some("`xargs` runs a command built from stdin".into()),
        ..Default::default()
    }
}

fn find(args: &[Tok]) -> Option<Unwrapped> {
    let mut i = 0;
    while let Some(s) = args.get(i).and_then(Tok::lit) {
        match s {
            "-H" | "-L" | "-P" => i += 1,
            "-D" => i += 2,
            _ if s.starts_with("-O") => i += 1,
            _ => break,
        }
    }
    let first_start = i.min(args.len());
    while let Some(tok) = args.get(i) {
        let expression = tok
            .lit()
            .is_some_and(|s| s.starts_with('-') || matches!(s, "(" | "!" | ","));
        if expression {
            break;
        }
        i += 1;
    }
    let starts = match args.get(first_start..i).unwrap_or_default() {
        [] => vec![Tok::Lit(".".into())],
        s => s.to_vec(),
    };
    let mut u = Unwrapped::default();
    while let Some(tok) = args.get(i) {
        i += 1;
        match tok.lit().unwrap_or_default() {
            action @ ("-exec" | "-execdir" | "-ok" | "-okdir") => {
                let end = args[i..]
                    .iter()
                    .position(|t| matches!(t.lit(), Some(";" | "+")))
                    .map_or(args.len(), |p| i + p);
                // `{}` is replaced by each found path.
                let cmd: Vec<Tok> = args[i..end]
                    .iter()
                    .map(|t| match t {
                        Tok::Lit(s) if s.contains("{}") => Tok::Dyn,
                        other => other.clone(),
                    })
                    .collect();
                if !cmd.is_empty() {
                    u.next.push(Next::Argv(cmd));
                }
                u.unknown_cwd |= action.ends_with("dir");
                u.unlisted = Some(format!("`find {action}` runs commands"));
                i = end + 1;
            }
            "-delete" => {
                // Deleting everything matched is at worst `rm -r` of each start point.
                let mut rm: Vec<Tok> = ["rm", "-r", "--"].map(|w| Tok::Lit(w.into())).into();
                rm.extend(starts.iter().cloned());
                u.next.push(Next::Argv(rm));
                u.unlisted = Some("`find -delete` removes files".into());
            }
            "-fprint" | "-fprint0" | "-fprintf" | "-fls" => {
                u.unlisted = Some("`find` writes to a file".into());
            }
            _ => {}
        }
    }
    u.unlisted.is_some().then_some(u)
}

fn parallel(args: &[Tok]) -> Unwrapped {
    let long_arg = "jobs sshlogin arg-file delimiter colsep joblog results tmpdir workdir wd";
    let start = scan_options(args, "jPSadInNLC", long_arg).0;
    let end = args[start..]
        .iter()
        .position(|t| t.lit().is_some_and(|s| s.starts_with(":::")))
        .map_or(args.len(), |p| start + p);
    let mut u = Unwrapped {
        unlisted: Some("`parallel` runs commands built from its inputs".into()),
        ..Default::default()
    };
    let Some(first) = inner(&args[..end], start) else {
        u.opaque = Some("`parallel` reading commands from stdin".into());
        return u;
    };
    // The template runs in a shell with each input substituted for its `{…}`
    // replacement strings, or appended when there are none; `"$@"` stands for them.
    for template in first.next.into_iter().filter_map(|n| match n {
        Next::Argv(t) => Some(t),
        Next::Script(_) => None,
    }) {
        match join_literal(&template) {
            Some(src) => u.next.push(Next::Script(substitute_placeholders(&src))),
            None => u.next.push(Next::Argv([template, vec![Tok::Dyn]].concat())),
        }
    }
    u.alternatives = first.alternatives;
    u
}

fn substitute_placeholders(template: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    let mut replaced = false;
    while let Some(open) = rest.find('{') {
        let Some(len) = rest[open..]
            .find('}')
            .filter(|&l| !rest[open..open + l].contains(char::is_whitespace))
        else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str("\"$@\"");
        rest = &rest[open + len + 1..];
        replaced = true;
    }
    out.push_str(rest);
    if !replaced {
        out.push_str(" \"$@\"");
    }
    out
}

fn watch(args: &[Tok]) -> Option<Unwrapped> {
    let (start, names) = scan_options(args, "nq", "interval equexit");
    let rest = args.get(start..).filter(|r| !r.is_empty())?;
    let mut u = if has(&names, &["x", "exec"]) {
        inner(args, start)?
    } else {
        // Without -x, watch passes the words to `sh -c`.
        match join_literal(rest) {
            Some(src) => Unwrapped {
                next: vec![Next::Script(src)],
                ..Default::default()
            },
            None => opaque("`watch` of a command only known at run time"),
        }
    };
    u.unlisted = Some("`watch` runs a command repeatedly".into());
    Some(u)
}

fn flock(args: &[Tok]) -> Option<Unwrapped> {
    let lock = scan_options(args, "wE", "timeout wait conflict-exit-code").0;
    match args.get(lock + 1..).unwrap_or_default() {
        [c, script, ..] if matches!(c.lit(), Some("-c" | "--command")) => Some(match script {
            Tok::Lit(src) => Unwrapped {
                next: vec![Next::Script(src.clone())],
                ..Default::default()
            },
            _ => opaque("`flock -c` with a command only known at run time"),
        }),
        _ => inner(args, lock + 1),
    }
}

fn ssh(args: &[Tok]) -> Unwrapped {
    let start = scan_options(args, "BbcDEeFIiJLlmOopQRSWw", "").0;
    let mut u = Unwrapped {
        unlisted: Some("`ssh` runs commands on another host".into()),
        unknown_cwd: true,
        ..Default::default()
    };
    let remote = args.get(start + 1..).unwrap_or_default();
    if remote.is_empty() {
        return u;
    }
    match join_literal(remote) {
        Some(src) => u.next.push(Next::Script(src)),
        None => u.opaque = Some("`ssh` with a remote command only known at run time".into()),
    }
    u
}
