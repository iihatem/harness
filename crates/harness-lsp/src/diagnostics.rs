//! What the model is told about the files an edit changed: the errors a language server found,
//! capped, or why there is nothing to say.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_core::diag::{Diagnostics, EditedFiles};
use lsp_types::DiagnosticSeverity;

use crate::{Launch, Manager, Report};

/// The most errors one edit's result lists.
pub const MAX_ERRORS: usize = 20;

/// The longest a message is shown.
const MAX_MESSAGE: usize = 300;

pub struct LspDiagnostics {
    manager: Manager,
    workspace: PathBuf,
}

impl LspDiagnostics {
    pub fn new(manager: Manager, workspace: PathBuf) -> LspDiagnostics {
        LspDiagnostics { manager, workspace }
    }

    fn shown(&self, path: &Path) -> String {
        one_line(
            &path
                .strip_prefix(&self.workspace)
                .unwrap_or(path)
                .display()
                .to_string(),
        )
    }
}

/// Characters that print nothing or reorder what is around them (Unicode categories Cf, Zl and Zp:
/// the bidi controls, zero-width characters, the line and paragraph separators, tags).
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{E0000}'..='\u{E007F}')
}

/// One line: control and invisible characters (a line break, an escape, a bidi override) become
/// spaces, and long text is cut.
fn one_line(message: &str) -> String {
    let flat: String = message
        .chars()
        .map(|c| {
            if c.is_control() || is_invisible(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_MESSAGE {
        let cut: String = flat.chars().take(MAX_MESSAGE).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// A header and up to `MAX_ERRORS` of `errors`, onto `lines`.
fn block(lines: &mut Vec<String>, qualifier: &str, errors: &[String]) {
    if errors.is_empty() {
        return;
    }
    let total = errors.len();
    lines.push(format!(
        "[diagnostics: {total} error{}{qualifier}]",
        if total == 1 { "" } else { "s" }
    ));
    lines.extend(errors.iter().take(MAX_ERRORS).cloned());
    if total > MAX_ERRORS {
        lines.push(format!(
            "[... {} more errors not shown]",
            total - MAX_ERRORS
        ));
    }
}

#[async_trait]
impl Diagnostics for LspDiagnostics {
    async fn after_edit(&self, edited: &EditedFiles<'_>) -> Option<String> {
        let launch = Launch {
            sandbox: edited.sandbox.clone(),
            access: edited.access,
            unsandboxed_ok: edited.unsandboxed_ok,
        };
        // The errors the server confirmed for this edit, and those of a set it did not republish.
        let mut errors: Vec<String> = Vec::new();
        let mut unchanged: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let list = |shown: &str, all: &[lsp_types::Diagnostic], into: &mut Vec<String>| {
            into.extend(
                all.iter()
                    .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                    .map(|d| {
                        format!(
                            "{shown}:{}: {}",
                            d.range.start.line + 1,
                            one_line(&d.message)
                        )
                    }),
            );
        };
        for path in edited.paths {
            match self.manager.check(path, &launch).await {
                Report::NoServer => {}
                Report::Note(note) => notes.push(format!("[{note}]")),
                Report::Pending => notes.push(format!(
                    "[diagnostics pending: the language server had not reported on {} in time]",
                    self.shown(path)
                )),
                Report::Checked(all) => list(&self.shown(path), &all, &mut errors),
                Report::Unchanged(all) => list(&self.shown(path), &all, &mut unchanged),
            }
        }
        let mut lines = Vec::new();
        block(&mut lines, "", &errors);
        block(&mut lines, ", unchanged since the last edit", &unchanged);
        lines.extend(notes);
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    fn reset(&self) {
        self.manager.reset();
    }

    async fn shutdown(&self) {
        self.manager.shutdown().await;
    }
}
