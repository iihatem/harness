//! Project context for harness: where a project starts, the instruction files (`AGENTS.md`,
//! `CLAUDE.md`) and environment facts that go into the system prompt, its assembly, and slash
//! commands.

pub mod commands;
pub mod environment;
pub mod instructions;
pub mod project;
pub mod prompt;
