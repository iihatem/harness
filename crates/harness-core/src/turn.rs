//! What a user turn sends: text, shell commands whose output is filled in before the message is
//! sent, and settings that apply to that turn only (a slash command's model and allowed tools).

use std::sync::Arc;

use crate::{engine::RuleSet, provider::Provider};

/// One piece of a turn's user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPart {
    Text(String),
    /// A shell command run before the message is sent; its output takes this part's place. It
    /// goes through the same permission check, approval, sandbox and guard as the `bash` tool.
    Shell(String),
}

/// A model that answers one turn instead of the session's model.
#[derive(Clone)]
pub struct TurnModel {
    pub provider: Arc<dyn Provider>,
    /// `<provider>/<model>`, recorded on its assistant messages.
    pub id: String,
    /// The model name sent to the provider.
    pub name: String,
}

impl std::fmt::Debug for TurnModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnModel").field("id", &self.id).finish()
    }
}

/// One user turn.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    pub parts: Vec<InputPart>,
    /// What the user typed, when the message sent differs from it (a slash command).
    pub display: Option<String>,
    /// Answers this turn instead of the session's model.
    pub model: Option<TurnModel>,
    /// Rules for this turn only. They never override deny rules, destructive-command
    /// confirmation or the sandbox.
    pub rules: RuleSet,
    /// Run this turn's shell commands in a read-only sandbox.
    pub read_only_shell: bool,
}

impl From<String> for TurnInput {
    fn from(text: String) -> Self {
        TurnInput {
            parts: vec![InputPart::Text(text)],
            ..TurnInput::default()
        }
    }
}

impl From<&str> for TurnInput {
    fn from(text: &str) -> Self {
        TurnInput::from(text.to_string())
    }
}
