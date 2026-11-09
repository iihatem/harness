//! The model a parent agent is on now, for a child that names none of its own to be built from.
//!
//! The session's model changes while it runs: `/model`, a role's model for one turn, a fallback,
//! an escalation. The agent publishes what it is using into a [`ParentModel`] each time, and the
//! frontend's factory reads it when a child is spawned, so the child never runs on the model the
//! user has left.

use std::sync::{Arc, Mutex, PoisonError};

use crate::{message::RequestOptions, provider::Provider, tool::ToolRegistry};

/// What a child that names no model is built from: the parent's model as it answers now.
#[derive(Clone)]
pub struct CurrentModel {
    pub provider: Arc<dyn Provider>,
    /// `<provider>/<model>`.
    pub id: String,
    /// The model name sent to the provider.
    pub name: String,
    pub context_window: u64,
    pub request: RequestOptions,
    pub text_tool_calls: bool,
    /// Whether it runs on a server of the user's own, which the concurrency limits count apart.
    pub local: bool,
    /// The tools the parent offers (its model's edit format included).
    pub tools: ToolRegistry,
    /// The parent's system prompt, its edit section as the model now has it.
    pub system_prompt: String,
    pub edit_section: Option<String>,
}

impl std::fmt::Debug for CurrentModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurrentModel")
            .field("id", &self.id)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

/// The models the roles other than `main` run on now: the configured ones, with what the session
/// has switched (`/model --role`) over them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleModels {
    pub plan: Option<String>,
    pub build: Option<String>,
    pub background: Option<String>,
}

impl RoleModels {
    /// The ids that are set.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        [&self.plan, &self.build, &self.background]
            .into_iter()
            .filter_map(|id| id.as_deref())
    }
}

/// Where the parent agent says which model it is on ([`Agent::with_model_cell`]
/// (crate::agent::Agent::with_model_cell)) and what its roles run on, and a factory or the tool
/// that starts subagents reads it.
#[derive(Clone, Default)]
pub struct ParentModel {
    current: Arc<Mutex<Option<CurrentModel>>>,
    roles: Arc<Mutex<RoleModels>>,
}

impl std::fmt::Debug for ParentModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParentModel")
            .field("current", &self.get())
            .field("roles", &self.roles())
            .finish()
    }
}

impl ParentModel {
    /// The parent's model now; `None` until the agent has published one.
    pub fn get(&self) -> Option<CurrentModel> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What the parent's roles run on now; nothing set until the agent has published them.
    pub fn roles(&self) -> RoleModels {
        self.roles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set(&self, model: CurrentModel) {
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(model);
    }

    pub(crate) fn set_roles(&self, roles: RoleModels) {
        *self.roles.lock().unwrap_or_else(PoisonError::into_inner) = roles;
    }
}
