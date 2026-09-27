//! Built-in destructive-command detection: recursive `rm` of anything but the
//! workspace interior, and history/work-tree destroying git operations (see
//! [`crate::git`]). An argument only known at run time that could change the
//! outcome counts as destructive.

use crate::argv::{Tok, command_name, display};
use crate::git;
use crate::paths::{Cwd, Workspace};

/// Returns why `argv` is destructive, or `None`.
pub(crate) fn check(argv: &[Tok], cwd: &Cwd, ws: &Workspace) -> Option<String> {
    let why = match command_name(argv.first()?.lit()?).as_str() {
        "rm" => rm(&argv[1..], cwd, ws)?,
        "git" => git::destructive(argv)?,
        _ => return None,
    };
    Some(format!("`{}` {why}", display(argv)))
}

fn rm(args: &[Tok], cwd: &Cwd, ws: &Workspace) -> Option<String> {
    let mut recursive = false;
    let mut maybe_option = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    for arg in args {
        match arg {
            Tok::Lit(s) if !options_done && s == "--" => options_done = true,
            Tok::Lit(s) if !options_done && s.starts_with("--") => {
                let name = s[2..].split('=').next().unwrap_or_default();
                if "no-preserve-root".starts_with(name) {
                    return Some("disables rm's root protection".into());
                }
                recursive |= "recursive".starts_with(name);
            }
            Tok::Lit(s) if !options_done && s.len() > 1 && s.starts_with('-') => {
                recursive |= s.contains(['r', 'R']);
            }
            Tok::Dyn if !options_done => {
                maybe_option = true;
                operands.push(arg);
            }
            _ => operands.push(arg),
        }
    }
    let verb = match (recursive, maybe_option) {
        (true, _) => "recursively deletes",
        // A run-time argument may turn out to be `-r`.
        (false, true) => "may recursively delete",
        (false, false) => return None,
    };
    operands
        .into_iter()
        .find_map(|op| unsafe_target(op, cwd, ws))
        .map(|what| format!("{verb} {what}"))
}

/// Describes why deleting `op` recursively is unsafe, or `None` if every location it
/// can name lies strictly inside the workspace.
fn unsafe_target(op: &Tok, cwd: &Cwd, ws: &Workspace) -> Option<String> {
    let path = match op {
        Tok::Dyn => return Some("a path only known at run time".into()),
        Tok::Glob { dir } if dir.is_empty() => ".",
        Tok::Glob { dir } => dir,
        Tok::Lit(p) => p,
    };
    cwd.resolve(path)
        .into_iter()
        .find_map(|resolved| match resolved {
            None => Some("a relative path under an unknown working directory".into()),
            Some(p) if p.as_path() == ws.root() => Some("the workspace root".into()),
            Some(p) if ws.root().starts_with(&p) => {
                Some(format!("`{}`, an ancestor of the workspace", p.display()))
            }
            Some(p) if !ws.strictly_contains(&p) => {
                Some(format!("`{}`, outside the workspace", p.display()))
            }
            Some(_) => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn destructive(cmd: &str, cwd: &str) -> bool {
        let argv: Vec<Tok> = cmd.split(' ').map(|w| Tok::Lit(w.into())).collect();
        let ws = Workspace::new(Path::new("/work/proj"));
        check(&argv, &Cwd::at(Path::new(cwd)), &ws).is_some()
    }

    #[test]
    fn rm_targets() {
        let ws = "/work/proj";
        assert!(destructive("rm -rf .", ws));
        assert!(destructive("rm -r -f ./", ws));
        assert!(destructive("rm dir -rf ../x", ws));
        assert!(destructive("rm --rec /", ws));
        assert!(destructive("rm -rf proj", "/work"));
        assert!(destructive("rm --no-preserve-root x", ws));
        assert!(!destructive("rm -rf target", ws));
        assert!(!destructive("rm ..", ws));
        assert!(!destructive("rm -- -rf", ws));
    }
}
