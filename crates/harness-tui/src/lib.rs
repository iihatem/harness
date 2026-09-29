//! harness's inline terminal UI: the conversation goes into the terminal's own scrollback, and
//! only a small live region at the bottom (the input, what is streaming, prompts) is redrawn.
//! This crate draws with `ratatui`; `harness-cli` sets up the session and starts it.

pub mod app;
pub mod approval;
pub mod complete;
pub mod diff;
pub mod editor;
pub mod highlight;
pub mod inline;
pub mod markdown;
pub mod status;
pub mod style;
pub mod terminal;
pub mod text;
pub mod transcript;
pub mod ui;
