//! The one-time proposal of a detected gate, asked when an interactive session starts in a
//! workspace with no gate configured. Headless runs never come here.

use std::{
    io::{BufRead, Write},
    path::Path,
};

use harness_config::{
    config,
    paths::Paths,
    trust::{GateAnswer, TrustStore},
};
use harness_core::gate::detect;
use harness_tui::approval::ARMING_DELAY;

use crate::{term::terminal_safe, trust::Typing};

/// The question.
const QUESTION: &str = "Use it? [y]es, [e]dit, or [N]o (not asked again):";

/// Said when what the user typed as the question appeared was thrown away.
const DISCARDED: &str =
    "What you typed was not taken as the answer: the question takes one once you pause.";

/// How the proposal went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proposed {
    /// A gate is configured, or was answered before, or nothing was detected: nothing was asked.
    NothingToAsk,
    /// Confirmed as proposed or as edited, and stored.
    Confirmed,
    /// Declined, and stored: it is not asked again.
    Declined,
}

/// Asks, once per workspace, whether to use the gate harness detects there. `answer` is where the
/// reply is read from. It is asked only when no gate is configured, nothing was answered before
/// and something was detected; only what the user types once they have seen the question and
/// paused counts (`typing`), as for the trust question. The reply is stored with the workspace's
/// trust record: `y` confirms, `e` edits each command first, and anything else declines.
pub fn propose(
    workspace: &Path,
    paths: &Paths,
    answer: &mut dyn BufRead,
    out: &mut dyn Write,
    typing: &mut dyn Typing,
) -> std::io::Result<Proposed> {
    let Ok(mut store) = TrustStore::load(&paths.data_dir) else {
        return Ok(Proposed::NothingToAsk);
    };
    // Settings that cannot be read are left for loading the configuration to report.
    let Ok(config) = config::load(&paths.global_config_file(), workspace, &store) else {
        return Ok(Proposed::NothingToAsk);
    };
    if config.gates.is_configured() || store.gate_answer(workspace).is_some() {
        return Ok(Proposed::NothingToAsk);
    }
    let Some(proposal) = detect(workspace) else {
        return Ok(Proposed::NothingToAsk);
    };
    typing.discard();
    writeln!(out, "No verification gate is configured. harness proposes:")?;
    writeln!(out, "  {}", proposal.describe())?;
    write!(out, "{QUESTION} ")?;
    out.flush()?;
    if typing.pause(ARMING_DELAY) {
        writeln!(out)?;
        writeln!(out, "{DISCARDED}")?;
        write!(out, "{QUESTION} ")?;
        out.flush()?;
    }
    let reply = read_line(answer)?;
    let stored = match reply.trim() {
        "y" | "Y" | "yes" => GateAnswer {
            confirmed: true,
            after_edit: proposal.after_edit,
            test: proposal.test,
        },
        "e" | "E" | "edit" => GateAnswer {
            confirmed: true,
            after_edit: edit(answer, out, "after-edit command", proposal.after_edit)?,
            test: edit(answer, out, "test command", proposal.test)?,
        },
        _ => GateAnswer::declined(),
    };
    let confirmed = stored.confirmed;
    store
        .set_gate_answer(workspace, stored)
        .map_err(std::io::Error::other)?;
    if confirmed {
        writeln!(out, "Gate saved. It is not asked again for this workspace.")?;
        Ok(Proposed::Confirmed)
    } else {
        writeln!(
            out,
            "No gate: none runs, and this is not asked again for this workspace. Set one in [gates] in {}.",
            terminal_safe(&config::project_file(workspace).display().to_string())
        )?;
        Ok(Proposed::Declined)
    }
}

/// Asks for the new text of one command, keeping `current` on an empty reply. Nothing is asked
/// of a command that was not proposed.
fn edit(
    answer: &mut dyn BufRead,
    out: &mut dyn Write,
    label: &str,
    current: Option<String>,
) -> std::io::Result<Option<String>> {
    let Some(current) = current else {
        return Ok(None);
    };
    write!(out, "  {label} [{current}] (Enter keeps it): ")?;
    out.flush()?;
    let typed = read_line(answer)?;
    let typed = typed.trim();
    Ok(Some(if typed.is_empty() {
        current
    } else {
        typed.to_string()
    }))
}

fn read_line(answer: &mut dyn BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    answer.read_line(&mut line)?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use harness_config::{config, trust::TrustStore};

    struct Fixture {
        _dir: tempfile::TempDir,
        ws: std::path::PathBuf,
        paths: Paths,
    }

    fn fixture(files: &[(&str, &str)], global: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for (name, text) in files {
            std::fs::write(ws.join(name), text).unwrap();
        }
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            state_dir: dir.path().join("state"),
        };
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.global_config_file(), global).unwrap();
        Fixture {
            _dir: dir,
            ws,
            paths,
        }
    }

    /// A user who types nothing until asked.
    struct Waits;

    impl Typing for Waits {
        fn discard(&mut self) {}
        fn pause(&mut self, _quiet: Duration) -> bool {
            false
        }
    }

    fn ask(f: &Fixture, reply: &str) -> (Proposed, String) {
        let mut out = Vec::new();
        let result = propose(&f.ws, &f.paths, &mut reply.as_bytes(), &mut out, &mut Waits).unwrap();
        (result, String::from_utf8(out).unwrap())
    }

    /// The gates a later session in the workspace runs.
    fn gates(f: &Fixture) -> harness_core::gate::Gates {
        let store = TrustStore::load(&f.paths.data_dir).unwrap();
        config::load(&f.paths.global_config_file(), &f.ws, &store)
            .unwrap()
            .gates
    }

    const CARGO: [(&str, &str); 1] = [("Cargo.toml", "[package]\nname = \"x\"\n")];

    // Spec "Proposal in a Rust workspace": it proposes `cargo test`, waits, and once confirmed
    // the test runs at the end of turns that changed files (the gate is configured from now on).
    #[test]
    fn a_rust_workspace_is_proposed_cargo_test_and_confirming_configures_it() {
        let f = fixture(&CARGO, "");
        let (result, shown) = ask(&f, "y\n");
        assert_eq!(result, Proposed::Confirmed);
        assert!(shown.contains("`cargo test`"), "{shown}");
        assert!(shown.contains("Cargo.toml"), "{shown}");
        assert_eq!(gates(&f).test.as_deref(), Some("cargo test"));
        assert_eq!(gates(&f).after_edit, None);
    }

    #[test]
    fn the_user_can_edit_the_proposal() {
        let f = fixture(&CARGO, "");
        let (result, shown) = ask(&f, "e\ncargo test --lib\n");
        assert_eq!(result, Proposed::Confirmed);
        assert!(shown.contains("test command [cargo test]"), "{shown}");
        assert_eq!(gates(&f).test.as_deref(), Some("cargo test --lib"));
    }

    #[test]
    fn editing_and_pressing_enter_keeps_what_was_proposed() {
        let f = fixture(
            &[(
                "package.json",
                r#"{"scripts": {"test": "jest", "lint": "eslint ."}}"#,
            )],
            "",
        );
        let (result, shown) = ask(&f, "e\n\nnpm run lint:fast\n");
        assert_eq!(result, Proposed::Confirmed);
        assert!(
            shown.contains("after-edit command [npm run lint]"),
            "{shown}"
        );
        let g = gates(&f);
        assert_eq!(g.after_edit.as_deref(), Some("npm run lint"));
        assert_eq!(g.test.as_deref(), Some("npm run lint:fast"));
    }

    // Spec "Declined once": no gate runs, and later sessions do not propose again.
    #[test]
    fn declining_is_stored_and_not_asked_again() {
        let f = fixture(&CARGO, "");
        let (result, shown) = ask(&f, "n\n");
        assert_eq!(result, Proposed::Declined);
        assert!(shown.contains("not asked again"), "{shown}");
        assert!(!gates(&f).is_configured());
        let (result, shown) = ask(&f, "y\n");
        assert_eq!(result, Proposed::NothingToAsk);
        assert_eq!(shown, "");
        assert!(!gates(&f).is_configured());
    }

    #[test]
    fn anything_but_yes_or_edit_declines() {
        for reply in ["\n", "no\n", "maybe\n", ""] {
            let f = fixture(&CARGO, "");
            assert_eq!(ask(&f, reply).0, Proposed::Declined, "{reply:?}");
        }
    }

    #[test]
    fn a_confirmed_proposal_is_not_asked_again_either() {
        let f = fixture(&CARGO, "");
        assert_eq!(ask(&f, "y\n").0, Proposed::Confirmed);
        assert_eq!(ask(&f, "n\n"), (Proposed::NothingToAsk, String::new()));
        assert_eq!(gates(&f).test.as_deref(), Some("cargo test"));
    }

    // Spec "No gates configured" / the proposal is for when no gate is configured.
    #[test]
    fn nothing_is_asked_when_a_gate_is_configured() {
        let f = fixture(&CARGO, "[gates]\ntest = \"make check\"\n");
        assert_eq!(ask(&f, "y\n"), (Proposed::NothingToAsk, String::new()));
        assert_eq!(gates(&f).test.as_deref(), Some("make check"));
    }

    #[test]
    fn nothing_is_asked_when_nothing_is_detected() {
        let f = fixture(&[("README.md", "hi")], "");
        assert_eq!(ask(&f, "y\n"), (Proposed::NothingToAsk, String::new()));
        // And nothing is stored, so a detectable file added later is proposed then.
        std::fs::write(f.ws.join("go.mod"), "module x\n").unwrap();
        assert_eq!(ask(&f, "y\n").0, Proposed::Confirmed);
        assert_eq!(gates(&f).test.as_deref(), Some("go test ./..."));
    }

    // Same rule as the trust question: what was typed before it showed does not answer it.
    #[test]
    fn what_was_typed_before_the_question_does_not_answer_it() {
        struct Typed(bool);
        impl Typing for Typed {
            fn discard(&mut self) {}
            fn pause(&mut self, _quiet: Duration) -> bool {
                self.0
            }
        }
        let f = fixture(&CARGO, "");
        let mut out = Vec::new();
        let result = propose(
            &f.ws,
            &f.paths,
            &mut "y\n".as_bytes(),
            &mut out,
            &mut Typed(true),
        )
        .unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert_eq!(result, Proposed::Confirmed);
        assert!(
            shown.contains("What you typed was not taken as the answer"),
            "{shown}"
        );
    }

    #[test]
    fn a_trust_file_that_cannot_be_read_asks_nothing() {
        let f = fixture(&CARGO, "");
        // The data directory's parent is a file: nothing can be stored.
        std::fs::write(f.paths.data_dir.parent().unwrap().join("blocker"), "x").unwrap();
        let paths = Paths {
            data_dir: f.paths.data_dir.parent().unwrap().join("blocker/data"),
            ..f.paths.clone()
        };
        let mut out = Vec::new();
        let result = propose(&f.ws, &paths, &mut "y\n".as_bytes(), &mut out, &mut Waits);
        // Loading the store fails first: nothing is asked, so nothing is half-done.
        assert_eq!(result.unwrap(), Proposed::NothingToAsk);
    }
}
