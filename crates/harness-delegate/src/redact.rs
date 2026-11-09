//! Redaction of what a child relays. The crate does it itself, with the caller's
//! [`Redactor`], so a secret a child prints cannot reach a display, a ledger or another model's
//! prompt through a call site that forgot to.

use std::sync::Arc;

use harness_core::redact::{Redactor, StreamRedactor};
use serde_json::Value;

use crate::event::{DelegateApproval, DelegateApprovalKind, DelegateEvent};

const LEFT_OUT: &str = "an event was left out because it could not be shown without a secret";

/// Redacts a run's events: the text of the deltas across events (a secret split over two deltas
/// is replaced whole), every other string on its own. What the delta streams hold back comes out
/// before the next event of another kind, and at [`finish`](Self::finish).
///
/// Approval `arguments` are left as they are (see [`DelegateApproval::arguments`]).
pub struct EventFilter {
    redactor: Arc<Redactor>,
    text: StreamRedactor,
    reasoning: StreamRedactor,
}

impl EventFilter {
    pub fn new(redactor: Arc<Redactor>) -> EventFilter {
        EventFilter {
            text: redactor.stream(),
            reasoning: redactor.stream(),
            redactor,
        }
    }

    /// `text` with every secret replaced (for a stderr tail or an error message).
    pub fn redact(&self, text: &str) -> String {
        self.redactor.redact(text)
    }

    /// The events to show for `event`: none while all of a delta's text is held back.
    pub fn push(&mut self, event: DelegateEvent) -> Vec<DelegateEvent> {
        match event {
            DelegateEvent::TextDelta { text } => delta(self.text.push(&text), |text| {
                DelegateEvent::TextDelta { text }
            }),
            DelegateEvent::ReasoningDelta { text } => delta(self.reasoning.push(&text), |text| {
                DelegateEvent::ReasoningDelta { text }
            }),
            // Numbers and windows; they can arrive while a reply streams.
            numbers @ (DelegateEvent::Usage { .. }
            | DelegateEvent::Cost { .. }
            | DelegateEvent::RateLimits { .. }) => vec![self.event(numbers)],
            other => {
                let mut shown = self.finish();
                shown.push(self.event(other));
                shown
            }
        }
    }

    /// What the delta streams still hold back.
    pub fn finish(&mut self) -> Vec<DelegateEvent> {
        let mut shown = delta(self.reasoning.finish(), |text| {
            DelegateEvent::ReasoningDelta { text }
        });
        shown.extend(delta(self.text.finish(), |text| DelegateEvent::TextDelta {
            text,
        }));
        shown
    }

    fn event(&self, event: DelegateEvent) -> DelegateEvent {
        match event {
            DelegateEvent::AssistantMessage { content, model } => DelegateEvent::AssistantMessage {
                content: self.redactor.redact_message(&content),
                model: model.map(|m| self.redactor.redact(&m)),
            },
            DelegateEvent::ApprovalNeeded(a) => DelegateEvent::ApprovalNeeded(self.approval(a)),
            other => self.through_json(other),
        }
    }

    fn approval(&self, a: DelegateApproval) -> DelegateApproval {
        let r = &self.redactor;
        DelegateApproval {
            id: a.id,
            call_id: a.call_id,
            tool: r.redact(&a.tool),
            arguments: a.arguments,
            kind: match a.kind {
                DelegateApprovalKind::Command { command, cwd } => DelegateApprovalKind::Command {
                    command: r.redact(&command),
                    cwd: cwd.map(|c| r.redact(&c)),
                },
                DelegateApprovalKind::FileChange { paths } => DelegateApprovalKind::FileChange {
                    paths: paths.iter().map(|p| r.redact(p)).collect(),
                },
                DelegateApprovalKind::Tool => DelegateApprovalKind::Tool,
            },
            reason: r.redact(&a.reason),
            can_remember: a.can_remember,
        }
    }

    /// Every string in the event, as JSON; the tag and the words of `WORD_FIELDS` are left alone.
    fn through_json(&self, event: DelegateEvent) -> DelegateEvent {
        let mut value: Value = serde_json::to_value(&event).expect("events serialize");
        self.redactor.redact_value(&mut value);
        serde_json::from_value(value).unwrap_or_else(|_| DelegateEvent::Warning {
            message: LEFT_OUT.into(),
        })
    }
}

fn delta(text: String, make: impl Fn(String) -> DelegateEvent) -> Vec<DelegateEvent> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![make(text)]
    }
}
