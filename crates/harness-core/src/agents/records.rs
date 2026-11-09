//! The records a parent keeps of its children: handed from the supervisor to the parent agent,
//! which saves them in its session beside the conversation.

use std::sync::{Arc, Mutex, PoisonError};

use crate::session::AgentRecord;

/// Records waiting to be saved in the parent's session. The supervisor adds to it from the
/// children's tasks; the parent agent takes from it at the end of each tool call and turn.
#[derive(Debug, Clone, Default)]
pub struct AgentRecords(Arc<Mutex<Vec<AgentRecord>>>);

impl AgentRecords {
    pub fn push(&self, record: AgentRecord) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record);
    }

    /// Takes everything added and not yet taken, oldest first.
    pub fn take(&self) -> Vec<AgentRecord> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
