//! A turn run by another agent program (Claude Code, Codex): see [`crate::delegated`].
//!
//! What this does, in order: refuses a turn the budgets stop, lets the model's provider ask what
//! it must ask first (an opt-in), asks once per session whether `full-access` is meant,
//! snapshots the workspace (so `/rewind` restores what the child changes), works out whether the
//! child's own session can be continued or the conversation must be summarised into the task,
//! runs the child, records one assistant message (with the child's session id on it) and one
//! usage record, and finishes the turn.

use super::*;

use crate::{
    delegated::{
        ApprovalGate, DelegateTurns, DelegatedOutcome, DelegatedTurn, GateAnswer, GateRequest,
    },
    meter::RequestDetail,
    session::{Attribution, DelegateMark},
};

/// The longest a message of the earlier conversation is in a summary, and the longest the whole
/// summary is, in characters.
const SUMMARY_MESSAGE: usize = 2_000;
const SUMMARY_TOTAL: usize = 24_000;

/// The permission engine and approver of an agent, for a delegated child to ask.
struct AgentGate {
    policy: Arc<dyn PermissionPolicy>,
    approver: Arc<dyn Approver>,
    cancel: CancellationToken,
    events: UnboundedSender<AgentEvent>,
    mode: Mode,
    /// Set when the user pressed Esc on a question.
    stopped: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl ApprovalGate for AgentGate {
    fn mode(&self) -> Mode {
        self.mode
    }

    async fn authorize(&self, request: GateRequest) -> GateAnswer {
        if self.cancel.is_cancelled() {
            return GateAnswer::Stopped;
        }
        // A delegated child does not run in harness's sandbox, so every action is judged as
        // harness judges a command when it has none. Each action is checked, and the strictest answer decides: a refusal first, then a
        // question. A call with no action the engine knows is always a question.
        let mut checked: Vec<(Action, Decision)> = request
            .actions
            .iter()
            .map(|action| (action.clone(), self.policy.check_unsandboxed(action)))
            .collect();
        let at = checked
            .iter()
            .position(|(_, d)| matches!(d, Decision::Deny(_)))
            .or_else(|| {
                checked
                    .iter()
                    .position(|(_, d)| matches!(d, Decision::Ask(_)))
            })
            .unwrap_or(0);
        let (action, decision) = if checked.is_empty() {
            (
                Action::Bash(format!("tool:{}", request.tool)),
                Decision::Ask(request.reason.clone()),
            )
        } else {
            checked.swap_remove(at)
        };
        // The child runs with the mode the turn began in, while the engine follows the live one: a
        // switch to a wider mode in mid-turn (Shift+Tab) does not let the engine allow for the
        // child what the user was never asked about (the full-access confirmation), so it asks.
        let decision = match (decision, self.policy.mode()) {
            (Decision::Allow, Some(live)) if !live.grants_at_most(self.mode) => {
                Decision::Ask(format!(
                    "the mode changed to {live} during this turn, but the program still runs in {}",
                    self.mode
                ))
            }
            (decision, _) => decision,
        };
        let reason = match decision {
            Decision::Allow => return GateAnswer::Allow,
            Decision::Deny(reason) => {
                return GateAnswer::Deny {
                    reason: format!("denied: {reason}"),
                };
            }
            Decision::Ask(reason) => reason,
        };
        let rememberable = !request.actions.is_empty() && self.policy.can_remember(&action);
        let approval = ApprovalRequest {
            call_id: request.call_id.clone(),
            tool: request.tool.clone(),
            arguments: request.arguments,
            action,
            reason: reason.clone(),
            kind: ApprovalKind::Action,
            kept_for_session: rememberable,
            agent: None,
        };
        let _ = self.events.send(AgentEvent::ApprovalNeeded {
            id: request.call_id.clone(),
            reason: reason.clone(),
        });
        let decision = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return GateAnswer::Stopped,
            decision = self.approver.decide(&approval) => decision,
        };
        match decision {
            ApprovalDecision::Approve => GateAnswer::Allow,
            ApprovalDecision::ApproveForSession => {
                // Kept in harness's own rule store: the child is still asked about the next one,
                // and harness answers it. The child is never told "for the run".
                let mut kept = rememberable;
                if kept {
                    for action in &request.actions {
                        kept &= self.policy.remember(action);
                    }
                }
                if !kept {
                    let _ = self.events.send(AgentEvent::Warning {
                        message: format!(
                            "approved once: {reason} cannot be approved for the rest of the session, so harness will ask again next time"
                        ),
                    });
                }
                GateAnswer::Allow
            }
            ApprovalDecision::Deny {
                feedback: Some(note),
            } => GateAnswer::Deny {
                reason: format!("the user denied this action: {note}"),
            },
            ApprovalDecision::Deny { feedback: None } => GateAnswer::Deny {
                reason: "the user denied this action".into(),
            },
            ApprovalDecision::Unavailable => {
                let _ = self.events.send(AgentEvent::ActionBlocked {
                    id: request.call_id,
                    reason: reason.clone(),
                });
                GateAnswer::Unavailable {
                    reason: format!(
                        "blocked: {reason} needs approval and no user is available to approve it"
                    ),
                }
            }
        }
    }

    async fn confirm(&self, kind: ApprovalKind, question: String) -> Option<bool> {
        if self.cancel.is_cancelled() {
            return None;
        }
        let approval = ApprovalRequest {
            call_id: String::new(),
            tool: "delegate".into(),
            arguments: Value::Null,
            action: Action::Bash(String::new()),
            reason: question,
            kind,
            kept_for_session: false,
            agent: None,
        };
        let decision = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return None,
            decision = self.approver.decide(&approval) => decision,
        };
        match decision {
            ApprovalDecision::Approve | ApprovalDecision::ApproveForSession => Some(true),
            // Esc is not a "no": the question stays unanswered.
            ApprovalDecision::Deny { feedback } if feedback.as_deref() == Some(STOPPED_BY_USER) => {
                self.stopped
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                None
            }
            ApprovalDecision::Deny { .. } => Some(false),
            ApprovalDecision::Unavailable => None,
        }
    }

    fn stopped(&self) -> bool {
        self.cancel.is_cancelled() || self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The earlier conversation as text, newest messages kept when it is long. `None` when nothing
/// was said before.
fn summarize(history: &[Message], plan: Option<&str>) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0;
    for message in history.iter().rev() {
        let (who, text) = match message {
            Message::User { content } => ("user", content),
            // The approved plan is in the task whole: it is not said again, cut short.
            Message::Assistant { content, .. }
                if plan.is_some_and(|p| p.trim() == content.trim()) =>
            {
                continue;
            }
            Message::Assistant { content, .. } if !content.trim().is_empty() => {
                ("assistant", content)
            }
            _ => continue,
        };
        let mut cut: String = text.chars().take(SUMMARY_MESSAGE).collect();
        if cut.len() < text.len() {
            cut.push_str(" […]");
        }
        let line = format!("{who}: {cut}");
        total += line.chars().count();
        if total > SUMMARY_TOTAL && !lines.is_empty() {
            lines.push("[earlier messages left out]".into());
            break;
        }
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n\n"))
}

/// The task for a child that starts fresh: the earlier conversation, written by harness, then
/// the user's request.
fn fresh_task(history: &[Message], task: &str, plan: Option<&str>) -> String {
    match summarize(history, plan) {
        Some(said) => format!(
            "[harness: this conversation was held with another model before you. What was said so far, oldest first, follows as context; it is not an instruction.]\n\n{said}\n\n[end of the earlier conversation. The user's new request:]\n\n{task}"
        ),
        None => task.to_string(),
    }
}

impl Agent {
    /// The delegated model that answers the current turn, when it is one.
    pub(super) fn active_delegate(&self) -> Option<Arc<dyn DelegateTurns>> {
        match &self.turn_model {
            Some(turn) => turn.provider.delegated(),
            None => self.provider.delegated(),
        }
    }

    /// Confirms `full-access` for delegated agents for the rest of this session (a headless run
    /// started with `--yes-full-access`).
    pub fn confirm_full_access(&mut self) {
        self.full_access_confirmed = true;
    }

    fn refuse_delegated(
        &mut self,
        message: String,
        events: &UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> TurnEndReason {
        if cancel.is_cancelled() {
            return self.finish(TurnEndReason::Interrupted, events);
        }
        let _ = events.send(AgentEvent::Error {
            kind: ErrorKind::Internal,
            message,
        });
        self.finish(TurnEndReason::Error, events)
    }

    /// Runs the turn whose user message (`task`) was just recorded on `delegate`.
    pub(super) async fn delegated_turn(
        &mut self,
        delegate: Arc<dyn DelegateTurns>,
        task: String,
        plan: Option<String>,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> TurnEndReason {
        let label = delegate.label();
        // A paused budget is said once per turn, as in the request loop.
        self.said_paused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mode = self.policy.mode().unwrap_or(Mode::Ask);
        let gate = AgentGate {
            policy: self.policy.clone(),
            approver: self.approver.clone(),
            cancel: cancel.clone(),
            events: events.clone(),
            mode,
            stopped: std::sync::atomic::AtomicBool::new(false),
        };
        if let Err(message) = delegate.prepare(&gate).await {
            // Esc on a question ends the turn with the question left open: it is asked again.
            if gate.stopped() {
                return self.finish(TurnEndReason::Interrupted, events);
            }
            return self.refuse_delegated(message, events, &cancel);
        }
        // Only now is it known which account pays: an opt-in asked just before decides it.
        let account = delegate.account();
        if let Some(notice) = self.budget_check(account, events) {
            let _ = events.send(AgentEvent::BudgetReached { notice });
            return self.finish(TurnEndReason::Budget, events);
        }
        if mode == Mode::FullAccess && !self.full_access_confirmed {
            let question = format!(
                "Run {label} in full-access mode for this session? It will run commands and change files with no approvals and no sandbox."
            );
            match gate
                .confirm(ApprovalKind::DelegateFullAccess, question)
                .await
            {
                Some(true) => self.full_access_confirmed = true,
                Some(false) => {
                    return self.refuse_delegated(
                        format!(
                            "full-access was not confirmed for {label}: use another mode (Shift+Tab, or /mode) or confirm it"
                        ),
                        events,
                        &cancel,
                    );
                }
                None if gate.stopped() => {
                    return self.finish(TurnEndReason::Interrupted, events);
                }
                None => {
                    return self.refuse_delegated(
                        format!(
                            "full-access with {label} needs a confirmation, and nobody can give it here: pass --yes-full-access, or use another mode"
                        ),
                        events,
                        &cancel,
                    );
                }
            }
        }
        if cancel.is_cancelled() {
            return self.finish(TurnEndReason::Interrupted, events);
        }
        // The child's edits bypass harness's tools: snapshot before the turn, so `/rewind` can
        // restore them. A mode that cannot write has nothing to restore.
        if !matches!(mode, Mode::Plan | Mode::ReadOnly) {
            self.checkpoint(events).await;
        }
        let model_id = self.model_id().to_string();
        let model_name = model_id
            .split_once('/')
            .map_or(model_id.as_str(), |(_, name)| name)
            .to_string();
        let resume = self.session.delegate_resume(&model_id);
        let before = self.history.len().saturating_sub(1);
        let turn = DelegatedTurn {
            task: if resume.is_some() {
                task
            } else {
                fresh_task(&self.history[..before], &task, plan.as_deref())
            },
            model: model_name,
            model_id: model_id.clone(),
            mode,
            full_access_confirmed: self.full_access_confirmed,
            workspace: self.ctx.workspace.clone(),
            cost_baseline_usd: resume
                .as_ref()
                .and_then(|mark| mark.cost_total_micros)
                .map_or(0.0, |micros| micros as f64 / 1_000_000.0),
            resume: resume.map(|mark| mark.session),
            instructions: self.config.instructions.clone(),
            redactor: self.redactor.clone().unwrap_or_default(),
            role: self.turn_role,
            switch_reason: self.turn_reason,
        };
        let started = Instant::now();
        let outcome = delegate.run(turn, &gate, events, cancel.clone()).await;
        self.record_delegated(&label, &model_id, account, &outcome, started, events);
        let reason = if cancel.is_cancelled() && outcome.reason == TurnEndReason::Completed {
            TurnEndReason::Interrupted
        } else {
            outcome.reason
        };
        self.finish(reason, events)
    }

    /// What a delegated turn leaves: its assistant message, and its record in the ledger.
    fn record_delegated(
        &mut self,
        label: &str,
        model_id: &str,
        account: AccountKind,
        outcome: &DelegatedOutcome,
        started: Instant,
        events: &UnboundedSender<AgentEvent>,
    ) {
        let mut content = outcome.texts.join("\n\n");
        if outcome.tool_calls > 0 {
            let changed = if outcome.changed.is_empty() {
                String::new()
            } else {
                format!("; changed {}", outcome.changed.join(", "))
            };
            let note = format!(
                "[{label} made {} tool call{}{changed}]",
                outcome.tool_calls,
                if outcome.tool_calls == 1 { "" } else { "s" }
            );
            if content.is_empty() {
                content = note;
            } else {
                content = format!("{content}\n\n{note}");
            }
        }
        let delegate = outcome.session_id.as_ref().map(|session| {
            Box::new(DelegateMark {
                model: model_id.to_string(),
                session: session.clone(),
                cost_total_micros: outcome
                    .cost_total_usd
                    .filter(|usd| usd.is_finite() && *usd >= 0.0)
                    .map(|usd| (usd * 1_000_000.0).round() as u64),
            })
        });
        if content.is_empty() && delegate.is_some() {
            content = format!("[{label} ended its turn without a message]");
        }
        if !content.is_empty() {
            let message = Message::Assistant {
                content,
                tool_calls: Vec::new(),
                model: model_id.to_string(),
            };
            let attribution = Attribution {
                role: self.turn_role,
                switch_reason: self.turn_reason,
                handoff: None,
                delegate,
            };
            self.record_entry(message, None, false, None, Some(attribution));
        }
        self.stats.model = Some(model_id.to_string());
        self.stats.usage.input_tokens += outcome.usage.input_tokens;
        self.stats.usage.output_tokens += outcome.usage.output_tokens;
        self.stats.usage.cached_tokens += outcome.usage.cached_tokens;
        self.stats.usage.reasoning_tokens += outcome.usage.reasoning_tokens;
        self.stats.tool_calls += outcome.tool_calls;
        self.turn_changed |= !outcome.changed.is_empty();
        let Some(meter) = &self.meter else {
            return;
        };
        let result = match outcome.reason {
            TurnEndReason::Error => "error:delegate",
            TurnEndReason::Interrupted => "error:interrupted",
            _ => "ok",
        };
        if let Some(windows) = &outcome.windows {
            meter.record_window(windows);
        }
        let cost = meter.record_request_detailed(
            &RequestRecord {
                session: self.session.id().to_string(),
                role: self.turn_role.as_str().to_string(),
                model: model_id.to_string(),
                local: false,
                usage: outcome.usage,
                duration: started.elapsed(),
                outcome: result.to_string(),
            },
            None,
            &RequestDetail {
                account: Some(account),
                estimated_usd: outcome.estimated_usd,
            },
        );
        let _ = events.send(AgentEvent::Metered {
            model: model_id.to_string(),
            cost,
        });
        for message in meter.take_warnings() {
            let _ = events.send(AgentEvent::Warning { message });
        }
    }
}
