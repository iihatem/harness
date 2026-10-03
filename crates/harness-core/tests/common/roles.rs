//! What the tests of roles, hand-off, fallback and escalation share: a resolver over fixed
//! models, and the events they look for.

#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use futures::future::BoxFuture;
use harness_core::{
    agent::Agent,
    event::AgentEvent,
    message::Message,
    role::{ModelResolver, Role, SwitchReason},
    session::{EntryKind, Session},
    testing::MockProvider,
    turn::TurnModel,
};
use tokio_util::sync::CancellationToken;

/// Resolves the ids it has a model for, and says which models a failed one falls back to.
pub struct Models(
    pub HashMap<String, TurnModel>,
    pub HashMap<String, Vec<String>>,
);

impl ModelResolver for Models {
    fn chain(&self, model_id: &str) -> Vec<String> {
        self.1.get(model_id).cloned().unwrap_or_default()
    }

    fn resolve(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<TurnModel, String>> {
        let found = self
            .0
            .get(id)
            .cloned()
            .ok_or_else(|| format!("{id} has no credentials"));
        Box::pin(async move { found })
    }
}

/// A model on `provider` with a context window of `window` tokens.
pub fn model(id: &str, provider: &Arc<MockProvider>, window: u64) -> TurnModel {
    TurnModel {
        provider: provider.clone(),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        local: false,
        tools: None,
        edit_section: None,
        context_window: Some(window),
        request: None,
        text_tool_calls: false,
    }
}

/// `agent` with a resolver that has `models`.
pub fn with_models(agent: Agent, models: Vec<TurnModel>) -> Agent {
    with_chain(agent, models, &[])
}

/// [`with_models`], with the chains `(failed id, candidates)`.
pub fn with_chain(agent: Agent, models: Vec<TurnModel>, chains: &[(&str, &[&str])]) -> Agent {
    agent.with_resolver(Arc::new(Models(
        models.into_iter().map(|m| (m.id.clone(), m)).collect(),
        chains
            .iter()
            .map(|(id, chain)| {
                (
                    id.to_string(),
                    chain.iter().map(|c| c.to_string()).collect(),
                )
            })
            .collect(),
    )))
}

/// The (from, to, role, reason) of each `ModelSwitched` event.
pub fn switches(events: &[AgentEvent]) -> Vec<(String, String, Role, SwitchReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelSwitched {
                from,
                to,
                role,
                reason,
                ..
            } => Some((from.clone(), to.clone(), *role, *reason)),
            _ => None,
        })
        .collect()
}

/// The user messages of `messages`, as text.
pub fn user_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

/// The entries of the session's active branch that hold a message.
pub fn message_entries(session: &Session) -> Vec<EntryKind> {
    session
        .branch()
        .into_iter()
        .filter(|e| matches!(e.kind, EntryKind::Message { .. }))
        .map(|e| e.kind.clone())
        .collect()
}
