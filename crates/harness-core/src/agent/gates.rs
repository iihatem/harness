//! Running verification gates: a gate command goes through the same path as a `bash` call of the
//! model's (the same permission check, approval, sandbox and guard), so a gate is never allowed
//! because the user wrote it into the configuration.

use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::Agent;
use crate::{
    event::{AgentEvent, GateKind, GateStatus, TurnEndReason},
    gate::{Outcome, outcome, tail},
    message::{Message, ToolCall},
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

/// What the end-of-turn gate has done in the current turn.
#[derive(Default)]
pub(super) struct GateTurn {
    /// Continuations a failed test has caused.
    retries: u32,
    /// The exit code and a hash of the output tail of the last failure.
    last_failure: Option<(Option<i32>, String)>,
    /// The test command was blocked, and the model was told.
    blocked: bool,
}

/// What happens when the model ends its turn.
pub(super) enum EndOfTurn {
    Finish(TurnEndReason),
    /// A gate result was added to the conversation: the model answers it.
    Continue,
}

impl Agent {
    /// The model ended its turn without calling a tool: runs the test gate when the turn changed
    /// files, and decides whether the turn is over.
    pub(super) async fn end_of_turn(
        &mut self,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> EndOfTurn {
        let done = EndOfTurn::Finish(TurnEndReason::Completed);
        let Some(command) = self.gates.test.clone() else {
            return done;
        };
        if self.gates_skipped() {
            let _ = events.send(AgentEvent::GateResult {
                gate: GateKind::Test,
                command: Some(command),
                status: GateStatus::Skipped,
                exit_code: None,
                tail: None,
            });
            return done;
        }
        if !self.turn_changed || self.gate_turn.blocked {
            return done;
        }
        let Some(run) = self.run_gate(GateKind::Test, &command, events).await else {
            return done;
        };
        if run.outcome.interrupted || cancel.is_cancelled() {
            return EndOfTurn::Finish(TurnEndReason::Interrupted);
        }
        match run.outcome.status {
            GateStatus::Passed => {
                // Changes the tests have passed on need no second run.
                self.turn_changed = false;
                self.gate_turn.last_failure = None;
                if self.deliver_steering(events) {
                    return EndOfTurn::Continue;
                }
                done
            }
            // The model is told once that the command could not run, which retrying would not
            // change; it is no test failure.
            GateStatus::Blocked | GateStatus::Skipped => {
                self.gate_turn.blocked = true;
                self.tell_gate_result(&run);
                self.deliver_steering(events);
                EndOfTurn::Continue
            }
            GateStatus::Failed | GateStatus::TimedOut => {
                let key = (
                    run.outcome.exit_code,
                    hex::encode(Sha256::digest(run.tail.as_bytes())),
                );
                let identical = self.gate_turn.last_failure.as_ref() == Some(&key);
                self.gate_turn.last_failure = Some(key);
                self.tell_gate_result(&run);
                if identical || self.gate_turn.retries >= self.gates.max_retries {
                    return EndOfTurn::Finish(TurnEndReason::GateFailed);
                }
                self.gate_turn.retries += 1;
                self.deliver_steering(events);
                EndOfTurn::Continue
            }
        }
    }

    /// Adds what the gate found to the conversation, for the model to read.
    fn tell_gate_result(&mut self, run: &GateRun) {
        let text = format!(
            "[harness] Gate result: {}",
            run.report(self.gates.timeout_s)
        );
        self.record(Message::User { content: text }, None, true);
    }
}
