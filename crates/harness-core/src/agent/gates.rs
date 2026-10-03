//! Running verification gates: a gate command goes through the same path as a `bash` call of the
//! model's (the same permission check, approval, sandbox and guard), so a gate is never allowed
//! because the user wrote it into the configuration.

use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use super::Agent;
use crate::{
    event::{AgentEvent, GateKind, GateStatus},
    gate::{Outcome, outcome, tail},
    message::ToolCall,
    output::spill,
    permission::{Action, Decision, Mode},
};

/// A gate command that ran, or was refused, and what to tell the model about it.
pub(super) struct GateRun {
    pub(super) kind: GateKind,
    pub(super) command: String,
    pub(super) outcome: Outcome,
    /// The last lines of the command's output, redacted, and how many lines came before them.
    pub(super) tail: String,
    pub(super) omitted: usize,
    /// Where the whole output was saved, when more than the tail was.
    pub(super) saved: Option<std::path::PathBuf>,
}

impl GateRun {
    /// What the model reads: a line saying how the gate ended, and, for a failure, the tail of
    /// the output.
    pub(super) fn report(&self, timeout_s: u64) -> String {
        let name = match self.kind {
            GateKind::AfterEdit => "after_edit",
            GateKind::Test => "test",
        };
        let command = &self.command;
        let head = match self.outcome.status {
            GateStatus::Passed => return format!("[{name} gate passed: `{command}`]"),
            GateStatus::Failed => match self.outcome.exit_code {
                Some(code) => format!("[{name} gate failed: `{command}` exit code {code}]"),
                None => format!("[{name} gate failed: `{command}` did not complete]"),
            },
            GateStatus::TimedOut => {
                format!("[{name} gate timed out after {timeout_s}s: `{command}` was stopped]")
            }
            GateStatus::Blocked => {
                let why = self.outcome.refusal.as_deref().unwrap_or("not approved");
                return format!("[{name} gate blocked: `{command}` did not run ({why})]");
            }
            GateStatus::Skipped => return format!("[{name} gate skipped: `{command}`]"),
        };
        let mut text = head;
        text.push('\n');
        if self.omitted > 0 {
            let saved = self
                .saved
                .as_ref()
                .map(|path| format!("; full output saved to {}", path.display()))
                .unwrap_or_default();
            text.push_str(&format!(
                "[... {} earlier lines omitted{saved}]\n",
                self.omitted
            ));
        }
        text.push_str(&self.tail);
        text
    }
}

impl Agent {
    /// Whether the approval mode (`plan`, `read-only`) forbids running any gate.
    pub(super) fn gates_skipped(&self) -> bool {
        match self.policy.mode() {
            Some(mode) => matches!(mode, Mode::Plan | Mode::ReadOnly),
            None => self.ctx.access == crate::permission::FsAccess::ReadOnly,
        }
    }

    /// After a successful call of an edit tool: notes that the turn changed files, and runs the
    /// `after_edit` command. What to append to the call's result, if anything.
    pub(super) async fn after_edit(
        &mut self,
        call: &ToolCall,
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<String> {
        let tool = self.tools.get(&call.name)?;
        let args = serde_json::from_str(&call.arguments).ok()?;
        if tool.changed_paths(&args, &self.ctx).is_empty() {
            return None;
        }
        self.turn_changed = true;
        let command = self.gates.after_edit.clone()?;
        if self.gates_skipped() {
            return None;
        }
        let run = self.run_gate(GateKind::AfterEdit, &command, events).await?;
        Some(run.report(self.gates.timeout_s))
    }

    /// Runs `command` as a `bash` call of the gate `kind`, and reports how it ended on `events`. The
    /// call is not shown as a tool call of its own: its result is the gate's.
    /// `None` when there is no `bash` tool to run it with.
    pub(super) async fn run_gate(
        &mut self,
        kind: GateKind,
        command: &str,
        events: &UnboundedSender<AgentEvent>,
    ) -> Option<GateRun> {
        self.tools.get("bash")?;
        self.gate_calls += 1;
        let call = ToolCall {
            id: format!("gate_{}", self.gate_calls),
            name: "bash".into(),
            arguments: json!({"command": command, "timeout_secs": self.gates.timeout_s})
                .to_string(),
        };
        // The permission check is the bash tool's; a rule that denies the command is a block,
        // reported as one like a command nobody could approve.
        let result = match self.policy.check(&Action::Bash(command.to_string())) {
            Decision::Deny(reason) => {
                let _ = events.send(AgentEvent::ActionBlocked {
                    id: call.id.clone(),
                    reason: reason.clone(),
                });
                crate::tool::ToolOutput::refused(format!("denied: {reason}"))
            }
            _ => self.execute_inner(&call, events).await,
        };
        let outcome = outcome(&result);
        // What the model gets is redacted whole before it is cut, so a secret the cut would run
        // through is not left in pieces.
        let redacted = match &self.redactor {
            Some(redactor) => redactor.redact(&outcome.output),
            None => outcome.output.clone(),
        };
        let (tail, omitted) = tail(&redacted, self.gates.output_tail_lines);
        let saved = (omitted > 0)
            .then(|| {
                spill(
                    &outcome.output,
                    &self.config.output_dir,
                    &call.id,
                    self.redactor.as_deref(),
                )
                .ok()
            })
            .flatten();
        let _ = events.send(AgentEvent::GateResult {
            gate: kind,
            command: Some(command.to_string()),
            status: outcome.status,
            exit_code: outcome.exit_code,
            tail: (outcome.status != GateStatus::Passed && !tail.is_empty()).then(|| tail.clone()),
        });
        Some(GateRun {
            kind,
            command: command.to_string(),
            outcome,
            tail,
            omitted,
            saved,
        })
    }
}
