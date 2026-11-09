//! Several agents in one process: who each is ([`identity`]), and, as later modules add them,
//! how they are limited, permitted, supervised and reported.

pub mod identity;
pub mod meter;
pub mod perm;
pub mod sched;

pub use identity::{AgentEnvelope, AgentId, AgentStatus, AgentTag, qualify_call_id};
pub use meter::ScopedMeter;
