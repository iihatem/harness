//! harness's inline terminal UI: the conversation goes into the terminal's own scrollback, and
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod style;
pub mod text;
