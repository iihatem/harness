use std::{
    io::{BufRead, IsTerminal, Write},
    path::Path,
};

use harness_config::{config, paths::Paths, trust::TrustStore};
use harness_context::project::project_root;
use harness_providers::credentials::Credentials;

use crate::term::terminal_safe;

/// The lines that list a workspace's widening settings, as `harness trust` and the interactive
/// session's first-use prompt show them.
pub fn describe_settings(workspace: &Path, items: &[String]) -> Vec<String> {
    let mut lines = vec![format!(
        "{} contains settings that need trust:",
        terminal_safe(&config::project_file(workspace).display().to_string())
    )];
    lines.extend(
        items
            .iter()
            .map(|item| format!("  - {}", terminal_safe(item))),
    );
    lines
}

/// How the first-use prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstUse {
    /// The workspace has no widening settings, or they are trusted already: nothing was asked.
    NothingToAsk,
    Trusted,
    Declined,
}

/// On interactive use of `workspace` while its project settings that widen what the agent may do
/// are not trusted, shows them on `out` and asks whether to trust the workspace, reading the
/// answer from `answer`. Trusting records it as `harness trust` does; declining is asked again
/// next time. Settings that cannot be read are left for loading the configuration to report.
pub fn first_use(
    workspace: &Path,
    paths: &Paths,
    answer: &mut dyn BufRead,
    out: &mut dyn Write,
) -> std::io::Result<FirstUse> {
    let Ok(widening) = config::project_widening(&paths.global_config_file(), workspace) else {
        return Ok(FirstUse::NothingToAsk);
    };
    let Ok(mut store) = TrustStore::load(&paths.data_dir) else {
        return Ok(FirstUse::NothingToAsk);
    };
    if widening.items.is_empty() || store.is_trusted(workspace, &widening.fingerprint) {
        return Ok(FirstUse::NothingToAsk);
    }
    for line in describe_settings(workspace, &widening.items) {
        writeln!(out, "{line}")?;
    }
    write!(out, "Trust this workspace, so these settings apply? [y/N] ")?;
    out.flush()?;
    let mut reply = String::new();
    answer.read_line(&mut reply)?;
    if !matches!(reply.trim(), "y" | "Y" | "yes") {
        writeln!(
            out,
            "Not trusted: harness runs without these settings. Run `harness trust` to review them again."
        )?;
        return Ok(FirstUse::Declined);
    }
    store
        .trust(workspace, &widening.fingerprint)
        .map_err(std::io::Error::other)?;
    writeln!(
        out,
        "Trusted {}.",
        terminal_safe(&workspace.display().to_string())
    )?;
    Ok(FirstUse::Trusted)
}

pub fn run(yes: bool, revoke: bool) -> u8 {
    let workspace = match std::env::current_dir().and_then(|d| d.canonicalize()) {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!(
                "error: cannot determine the working directory: {}",
                terminal_safe(&e.to_string())
            );
            return 2;
        }
    };
    let paths = match Paths::from_process_env() {
        Ok(paths) => paths,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 2;
        }
    };
    let mut store = match TrustStore::load(&paths.data_dir) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            return 1;
        }
    };
    if revoke {
        return match store.revoke(&workspace) {
            Ok(true) => {
                println!(
                    "Revoked trust for {}.",
                    terminal_safe(&workspace.display().to_string())
                );
                0
            }
            Ok(false) => {
                println!(
                    "{} was not trusted.",
                    terminal_safe(&workspace.display().to_string())
                );
                0
            }
            Err(e) => {
                eprintln!("error: {}", terminal_safe(&e.to_string()));
                1
            }
        };
    }
    let widening = match config::project_widening(&paths.global_config_file(), &workspace) {
        Ok(widening) => widening,
        Err(e) => {
            // What cannot be read can hold a key.
            let credentials = Credentials::open(&paths.data_dir, crate::setup::env);
            let redactor = crate::setup::known_secrets(&credentials);
            eprintln!("error: {}", terminal_safe(&redactor.redact(&e.to_string())));
            return 2;
        }
    };
    // Command files come from the project root: trust given here covers them only there.
    let root = project_root(&workspace);
    if widening.items.is_empty() && root == workspace {
        // Trust still matters: a trusted workspace's command files may choose their model.
        println!(
            "No project settings in {} widen what the agent may do. Trusting it lets its command files choose their model, until such settings appear.",
            terminal_safe(&workspace.display().to_string())
        );
    } else if widening.items.is_empty() {
        println!(
            "No project settings in {} widen what the agent may do.",
            terminal_safe(&workspace.display().to_string())
        );
    } else {
        for line in describe_settings(&workspace, &widening.items) {
            println!("{line}");
        }
    }
    if root != workspace {
        println!(
            "Trusting {} does not cover the command files used there: they come from {}; run `harness trust` there to let them choose their model.",
            terminal_safe(&workspace.display().to_string()),
            terminal_safe(&root.display().to_string())
        );
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("error: stdin is not a terminal; re-run with --yes to trust this workspace");
            return 2;
        }
        print!("Trust this workspace? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("Not trusted.");
            return 0;
        }
    }
    match store.trust(&workspace, &widening.fingerprint) {
        Ok(()) => {
            println!(
                "Trusted {}.",
                terminal_safe(&workspace.display().to_string())
            );
            0
        }
        Err(e) => {
            eprintln!("error: {}", terminal_safe(&e.to_string()));
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Workspace {
        _dir: tempfile::TempDir,
        ws: std::path::PathBuf,
        paths: Paths,
    }

    fn workspace(project: &str) -> Workspace {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        if !project.is_empty() {
            std::fs::write(ws.join(".harness/config.toml"), project).unwrap();
        }
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            state_dir: dir.path().join("state"),
        };
        Workspace {
            _dir: dir,
            ws,
            paths,
        }
    }

    fn ask(w: &Workspace, reply: &str) -> (FirstUse, String) {
        let mut out = Vec::new();
        let result = first_use(&w.ws, &w.paths, &mut reply.as_bytes(), &mut out).unwrap();
        (result, String::from_utf8(out).unwrap())
    }

    fn allowed(w: &Workspace) -> Vec<String> {
        let trust = TrustStore::load(&w.paths.data_dir).unwrap();
        config::load(&w.paths.global_config_file(), &w.ws, &trust)
            .unwrap()
            .allow
    }

    const ALLOW: &str = "[permissions]\nallow = [\"bash:make *\"]\n";

    #[test]
    fn trusting_on_first_use_applies_the_settings() {
        let w = workspace(ALLOW);
        assert!(allowed(&w).is_empty());
        let (result, shown) = ask(&w, "y\n");
        assert_eq!(result, FirstUse::Trusted);
        assert!(
            shown.contains("contains settings that need trust:"),
            "{shown}"
        );
        assert!(
            shown.contains("  - permissions.allow: \"bash:make *\""),
            "{shown}"
        );
        assert!(shown.contains("Trust this workspace, so these settings apply? [y/N]"));
        assert_eq!(allowed(&w), ["bash:make *"]);
        // Asked once: trusted now.
        assert_eq!(ask(&w, "").0, FirstUse::NothingToAsk);
    }

    #[test]
    fn declining_leaves_them_off_and_asks_again_next_time() {
        let w = workspace(ALLOW);
        let (result, shown) = ask(&w, "\n");
        assert_eq!(result, FirstUse::Declined);
        assert!(shown.contains("Run `harness trust`"), "{shown}");
        assert!(allowed(&w).is_empty());
        assert_eq!(ask(&w, "no\n").0, FirstUse::Declined);
    }

    #[test]
    fn nothing_is_asked_without_widening_settings() {
        let w = workspace("[permissions]\ndeny = [\"bash:curl *\"]\n");
        assert_eq!(ask(&w, "y\n"), (FirstUse::NothingToAsk, String::new()));
        let w = workspace("");
        assert_eq!(ask(&w, "y\n"), (FirstUse::NothingToAsk, String::new()));
    }

    #[test]
    fn settings_from_the_repository_are_shown_escaped() {
        let w = workspace("[permissions]\nallow = [\"bash:\\u001b[2Jx\"]\n");
        let (_, shown) = ask(&w, "n\n");
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
    }
}
