//! Model roles: which model does which work. `main` answers everything not named below, `plan`
//! the turns in `plan` mode, `build` the turn Build starts, and `background` compaction. A role
//! that is not set uses `main`.

use std::{fmt, str::FromStr};

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::turn::TurnModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Main,
    Plan,
    Build,
    Background,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Main, Role::Plan, Role::Build, Role::Background];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Main => "main",
            Role::Plan => "plan",
            Role::Build => "build",
            Role::Background => "background",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    /// The error names the roles, for `/model --role` to show.
    fn from_str(s: &str) -> Result<Role, String> {
        Role::ALL
            .into_iter()
            .find(|role| role.as_str() == s)
            .ok_or_else(|| {
                format!("unknown role `{s}`; the roles are main, plan, build and background")
            })
    }
}

/// Why a model switched, or why a message is on a model other than the one its role is configured
/// to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchReason {
    /// A configured fallback chain moved a failed request.
    Fallback,
    /// The user escalated (`/escalate`).
    Escalation,
    /// The user's command or key: `/model`, `/model --role`, Shift+Tab into `plan` mode, Build, or
    /// a command file's `model:`.
    User,
}

impl SwitchReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SwitchReason::Fallback => "fallback",
            SwitchReason::Escalation => "escalation",
            SwitchReason::User => "user",
        }
    }
}

/// The line that announces a model switch, for the terminal and for `harness ask`'s stderr.
pub fn switched_text(
    from: &str,
    to: &str,
    role: Role,
    reason: SwitchReason,
    detail: Option<&str>,
) -> String {
    let why = match reason {
        SwitchReason::User => "you chose it",
        SwitchReason::Fallback => "fallback",
        SwitchReason::Escalation => "escalation",
    };
    let mut text = format!("switched to {to} ({role} role, from {from}; {why})");
    if let Some(detail) = detail {
        text.push_str(": ");
        text.push_str(detail);
    }
    text
}

/// Where a role's model comes from, as `/roles` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// The configuration (`[roles]`, `model`, or `--model`).
    Config,
    /// Set in this session (`/model`, `/model --role`).
    Session,
    /// Not set: the role uses `main`.
    Inherited,
}

impl RoleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleSource::Config => "config",
            RoleSource::Session => "session",
            RoleSource::Inherited => "inherited",
        }
    }
}

/// One row of `/roles`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleLine {
    pub role: Role,
    /// The model the role runs on now: `<provider>/<model>`.
    pub model: String,
    pub source: RoleSource,
}

/// How the Build turn gets the conversation, when `[roles.handoff] mode` forces it. Unset, the
/// history goes along when it fits the build model's window and the plan alone when it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    /// The whole conversation, which holds the approved plan.
    History,
    /// The system prompt, the instruction files and the approved plan only.
    PlanOnly,
}

impl HandoffMode {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffMode::History => "history",
            HandoffMode::PlanOnly => "plan_only",
        }
    }
}

/// How a Build turn got the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffKind {
    /// The build model is the one that planned: the turn goes on as it would have.
    SameModel,
    /// Another model, with the whole conversation.
    History,
    /// Another model, with the plan alone.
    PlanOnly,
}

impl HandoffKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HandoffKind::SameModel => "same_model",
            HandoffKind::History => "history",
            HandoffKind::PlanOnly => "plan_only",
        }
    }
}

/// What a Build turn's hand-off was, as the session records it on the Build message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    pub kind: HandoffKind,
    /// `[roles.handoff] mode` chose it.
    pub forced: bool,
    /// The estimated tokens of the conversation before the Build message.
    pub history_tokens: u64,
    /// The build model's effective context window.
    pub window: u64,
}

/// The notice that a Build turn goes with the plan alone.
pub fn handoff_reduced_text(to: &str, history_tokens: u64, window: u64, forced: bool) -> String {
    let what = "the system prompt, the instruction files and the approved plan";
    if forced {
        format!(
            "[roles.handoff] mode = \"plan_only\" is set, so the build turn on {to} gets the plan alone: {what}"
        )
    } else {
        format!(
            "the conversation (about {history_tokens} tokens) does not fit {to}'s {window}-token window, so the build turn gets the plan alone: {what}"
        )
    }
}

/// The roles as the configuration sets them: model ids, and the hand-off mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleConfig {
    pub main: Option<String>,
    pub plan: Option<String>,
    pub build: Option<String>,
    pub background: Option<String>,
    pub handoff: Option<HandoffMode>,
}

impl RoleConfig {
    /// The configured model of `role`.
    pub fn get(&self, role: Role) -> Option<&String> {
        match role {
            Role::Main => self.main.as_ref(),
            Role::Plan => self.plan.as_ref(),
            Role::Build => self.build.as_ref(),
            Role::Background => self.background.as_ref(),
        }
    }
}

/// What the session has set for the roles other than `main`: the configured models, over which
/// `/model --role` puts the session's own.
#[derive(Debug, Clone, Default)]
pub(crate) struct RoleTable {
    slots: [Option<(String, RoleSource)>; 3],
    pub(crate) handoff: Option<HandoffMode>,
}

impl RoleTable {
    pub(crate) fn from_config(config: &RoleConfig) -> RoleTable {
        let slot = |id: &Option<String>| id.clone().map(|id| (id, RoleSource::Config));
        RoleTable {
            slots: [
                slot(&config.plan),
                slot(&config.build),
                slot(&config.background),
            ],
            handoff: config.handoff,
        }
    }

    fn index(role: Role) -> Option<usize> {
        match role {
            Role::Main => None,
            Role::Plan => Some(0),
            Role::Build => Some(1),
            Role::Background => Some(2),
        }
    }

    pub(crate) fn get(&self, role: Role) -> Option<&(String, RoleSource)> {
        RoleTable::index(role).and_then(|i| self.slots[i].as_ref())
    }

    /// Sets `role`'s model for the session; `false` for `main`, which the session's own model is.
    pub(crate) fn set(&mut self, role: Role, id: &str) -> bool {
        match RoleTable::index(role) {
            Some(i) => {
                self.slots[i] = Some((id.to_string(), RoleSource::Session));
                true
            }
            None => false,
        }
    }
}

/// Makes a model id ready for use: its provider, with the credentials it needs, and its whole
/// profile. The frontend's side of roles: the agent asks for a role's model when a turn needs it,
/// and a failure is told to the user, whose turn then does not run.
pub trait ModelResolver: Send + Sync {
    /// The model `id` (`<provider>/<model>`). The error says why it cannot be used. `cancel`
    /// stops the wait, say for a local server that is loading the model.
    fn resolve(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<TurnModel, String>>;

    /// The models a failed request on `model_id` may be sent to, in order: the `[fallback]` chain
    /// whose glob matches it, none when no chain does.
    fn chain(&self, _model_id: &str) -> Vec<String> {
        Vec::new()
    }
}
