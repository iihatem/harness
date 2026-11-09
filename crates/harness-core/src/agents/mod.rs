//! Several agents in one process: who each is ([`identity`]), how many run at once ([`sched`]),
//! what each may do ([`perm`]), what it reports ([`report`]), how its usage is recorded
//! ([`meter`]) and the [`supervisor`] that spawns and tracks them.

pub mod identity;
pub mod meter;
pub mod model;
pub mod perm;
pub mod records;
pub mod report;
pub mod sched;
pub mod supervisor;
pub mod workspace;

pub use identity::{AgentEnvelope, AgentId, AgentStatus, AgentTag, qualify_call_id};
pub use meter::ScopedMeter;
pub use model::{CurrentModel, ParentModel};
pub use records::AgentRecords;
pub use report::{AgentReport, ReportFlag, ToolLog};
pub use sched::{Endpoint, Limits};
pub use supervisor::{
    AgentFactory, AgentHandle, AgentInfo, AgentSpec, ChildSetup, ParentContext, Supervisor,
};
pub use workspace::{ChildWorkspace, WorkspaceRefused};
