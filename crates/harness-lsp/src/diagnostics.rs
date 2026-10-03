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
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// One line: control characters (a line break, an escape) become spaces, and long text is cut.
fn one_line(message: &str) -> String {
    let flat: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_MESSAGE {
        let cut: String = flat.chars().take(MAX_MESSAGE).collect();
        format!("{cut}…")
    } else {
        flat
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
        let mut errors: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        for path in edited.paths {
            match self.manager.check(path, &launch).await {
                Report::NoServer => {}
                Report::Note(note) => notes.push(format!("[{note}]")),
                Report::Pending => notes.push(format!(
                    "[diagnostics pending: the language server had not reported on {} in time]",
                    self.shown(path)
                )),
                Report::Checked(all) => {
                    let shown = self.shown(path);
                    errors.extend(
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
                }
            }
        }
        let mut lines = Vec::new();
        if !errors.is_empty() {
            let total = errors.len();
            lines.push(format!(
                "[diagnostics: {total} error{}]",
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
