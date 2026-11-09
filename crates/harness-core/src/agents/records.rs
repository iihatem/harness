//! The records a parent keeps of its children: handed from the supervisor to the parent agent,
//! which saves them in its session beside the conversation.
//!
//! Each record names the parent session it belongs to (the one the child was spawned in), so a
//! session that moved on (`/new`, `/resume`) never gets the records of the one it left. The queue
//! also counts the children that have not reported yet, so the parent, at the end of the session,
//! can wait for those still ending before it saves what is left.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::sync::watch;

use crate::session::{AgentRecord, AgentRecordKind};

/// The most records of other sessions that stay queued.
const KEPT: usize = 256;

struct Shared {
    queue: Mutex<Vec<(String, AgentRecord)>>,
    /// Children spawned that have not pushed their `Reported` record yet.
    unreported: watch::Sender<usize>,
}

/// Records waiting to be saved in the parent's session. The supervisor adds to it from the
/// children's tasks; the parent agent takes from it at the end of each tool call and turn, when
/// it moves to another session and when its session ends.
#[derive(Clone)]
pub struct AgentRecords(Arc<Shared>);

impl Default for AgentRecords {
    fn default() -> Self {
        AgentRecords(Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            unreported: watch::channel(0).0,
        }))
    }
}

impl std::fmt::Debug for AgentRecords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRecords")
            .field("unreported", &*self.0.unreported.borrow())
            .finish_non_exhaustive()
    }
}

impl AgentRecords {
    /// A child was spawned: its `Reported` record is expected.
    pub fn expect_report(&self) {
        self.0.unreported.send_modify(|n| *n += 1);
    }

    /// Adds `record`, which belongs to the parent session `session`. A `Reported` one is the
    /// child's last.
    pub fn push(&self, session: &str, record: AgentRecord) {
        let reported = record.kind == AgentRecordKind::Reported;
        self.0
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((session.to_string(), record));
        if reported {
            self.0.unreported.send_modify(|n| *n = n.saturating_sub(1));
        }
    }

    /// Takes the records of the parent session `session` that were added and not yet taken,
    /// oldest first. Those of any other session stay queued: a background child can report after
    /// its session was left with `/new`, and `/resume` of that session in the same run then
    /// finds the report. (A run is short and a record small; the queue holds the newest
    /// [`KEPT`] of the others at most.)
    pub fn take(&self, session: &str) -> Vec<AgentRecord> {
        let mut queue = self.0.queue.lock().unwrap_or_else(PoisonError::into_inner);
        let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut *queue)
            .into_iter()
            .partition(|(owner, _)| owner == session);
        let skip = others.len().saturating_sub(KEPT);
        *queue = others.into_iter().skip(skip).collect();
        mine.into_iter().map(|(_, record)| record).collect()
    }

    /// Waits until every child spawned has reported, or `limit` has passed.
    pub async fn settled(&self, limit: Duration) {
        let mut count = self.0.unreported.subscribe();
        let _ = tokio::time::timeout(limit, count.wait_for(|n| *n == 0)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(agent: &str) -> AgentRecord {
        AgentRecord {
            agent: agent.into(),
            label: "general".into(),
            kind: AgentRecordKind::Reported,
            session: None,
            report: None,
        }
    }

    #[test]
    fn what_a_session_left_waits_for_it_to_come_back() {
        let records = AgentRecords::default();
        records.push("old", record("a1"));
        records.push("new", record("a2"));
        let new: Vec<_> = records.take("new").into_iter().map(|r| r.agent).collect();
        assert_eq!(new, ["a2"]);
        // The session that was left is resumed in the same run: the report is there.
        let old: Vec<_> = records.take("old").into_iter().map(|r| r.agent).collect();
        assert_eq!(old, ["a1"]);
        assert!(records.take("old").is_empty());
    }
}
