//! Who an agent is, and how an event says which agent it is from.
//!
//! The main agent has no id. A child has one (`a1`, `a2`, ...), unique in its supervisor, and a
//! label to show. Events are not changed: a child's events travel in an [`AgentEnvelope`], whose
//! JSON is the event's own line plus an `agent` field (left out for the main agent, so existing
//! consumers of `ask --json` see no difference).

use serde::{Deserialize, Serialize};

use crate::event::AgentEvent;

/// A child agent's id, unique within one supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// An agent's id and the name shown for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTag {
    pub id: AgentId,
    pub label: String,
}

/// Where a child agent is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Waiting for a free place under the concurrency limits.
    Queued,
    Working,
    /// Waiting for someone to answer an approval.
    NeedsInput,
    Done,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// Whether the agent has ended.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            AgentStatus::Done | AgentStatus::Failed | AgentStatus::Cancelled
        )
    }

    /// The word shown for it.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Queued => "queued",
            AgentStatus::Working => "working",
            AgentStatus::NeedsInput => "needs input",
            AgentStatus::Done => "done",
            AgentStatus::Failed => "failed",
            AgentStatus::Cancelled => "cancelled",
        }
    }
}

/// The id a child's tool call has outside the child: `<agent>/<call id>`, so calls of different
/// agents never share one.
pub fn qualify_call_id(agent: &AgentId, call_id: &str) -> String {
    format!("{agent}/{call_id}")
}

/// An event and the agent it is from (`None`: the main agent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEnvelope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
    #[serde(flatten)]
    pub event: AgentEvent,
}

impl AgentEnvelope {
    /// An event of the main agent.
    pub fn main(event: AgentEvent) -> Self {
        AgentEnvelope { agent: None, event }
    }

    /// An event of child `agent`, with the ids of tool calls qualified by it.
    pub fn child(agent: &AgentId, mut event: AgentEvent) -> Self {
        match &mut event {
            AgentEvent::ToolCallRequested { id, .. }
            | AgentEvent::ApprovalNeeded { id, .. }
            | AgentEvent::ActionBlocked { id, .. }
            | AgentEvent::ToolCallFinished { id, .. } => *id = qualify_call_id(agent, id),
            _ => {}
        }
        AgentEnvelope {
            agent: Some(agent.clone()),
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn requested(id: &str) -> AgentEvent {
        AgentEvent::ToolCallRequested {
            id: id.into(),
            name: "bash".into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn a_main_event_prints_exactly_as_before() {
        let event = requested("call_h0");
        let plain = serde_json::to_value(&event).unwrap();
        let wrapped = serde_json::to_value(AgentEnvelope::main(event)).unwrap();
        assert_eq!(plain, wrapped);
        assert!(wrapped.get("agent").is_none());
    }

    #[test]
    fn a_child_event_adds_agent_and_qualifies_its_call_id() {
        let line = serde_json::to_value(AgentEnvelope::child(
            &AgentId("a1".into()),
            requested("call_h0"),
        ))
        .unwrap();
        assert_eq!(line["agent"], json!("a1"));
        assert_eq!(line["type"], json!("tool_call_requested"));
        assert_eq!(line["id"], json!("a1/call_h0"));
    }

    #[test]
    fn two_agents_with_the_same_call_id_do_not_collide() {
        let a = AgentEnvelope::child(&AgentId("a1".into()), requested("call_h0"));
        let b = AgentEnvelope::child(&AgentId("a2".into()), requested("call_h0"));
        assert_ne!(a.event, b.event);
    }

    #[test]
    fn every_event_that_names_a_call_is_qualified() {
        let id = AgentId("a3".into());
        for event in [
            AgentEvent::ApprovalNeeded {
                id: "c".into(),
                reason: "r".into(),
            },
            AgentEvent::ActionBlocked {
                id: "c".into(),
                reason: "r".into(),
            },
            AgentEvent::ToolCallFinished {
                id: "c".into(),
                output: "o".into(),
                is_error: false,
            },
        ] {
            let json = serde_json::to_value(AgentEnvelope::child(&id, event)).unwrap();
            assert_eq!(json["id"], json!("a3/c"));
        }
    }

    #[test]
    fn lines_round_trip_and_old_lines_without_agent_still_parse() {
        let child = AgentEnvelope::child(&AgentId("a1".into()), AgentEvent::TurnStarted);
        let text = serde_json::to_string(&child).unwrap();
        assert_eq!(serde_json::from_str::<AgentEnvelope>(&text).unwrap(), child);
        let old: AgentEnvelope = serde_json::from_str(r#"{"type":"turn_started"}"#).unwrap();
        assert_eq!(old, AgentEnvelope::main(AgentEvent::TurnStarted));
    }

    #[test]
    fn lifecycle_events_say_who_and_how_it_ended() {
        let id = AgentId("a1".into());
        let started = AgentEvent::AgentStarted {
            label: "explore".into(),
            model: "m/x".into(),
        };
        let finished = AgentEvent::AgentFinished {
            status: AgentStatus::Done,
            summary: "ok".into(),
        };
        let s = serde_json::to_value(AgentEnvelope::child(&id, started)).unwrap();
        let f = serde_json::to_value(AgentEnvelope::child(&id, finished)).unwrap();
        assert_eq!(s["type"], json!("agent_started"));
        assert_eq!(f["status"], json!("done"));
        assert_eq!(f["agent"], json!("a1"));
    }
}
