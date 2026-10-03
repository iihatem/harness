//! A small language-server client over stdio, for the errors a server finds in a file the agent
//! edited: it starts a server, tells it about the file's new text, and waits a while for the
//! diagnostics it publishes.

mod client;
mod diagnostics;
mod frame;
mod manager;

pub use client::{Check, Client, LspError, uri_of};
pub use diagnostics::{LspDiagnostics, MAX_ERRORS};
pub use lsp_types::{Diagnostic, DiagnosticSeverity};
pub use manager::{
    Found, Language, Launch, Manager, NO_SANDBOX_NOTE, Report, ServerSetting, Settings,
    UNTRUSTED_NOTE, language_of,
};
