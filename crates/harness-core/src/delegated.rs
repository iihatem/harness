//! Models that run in another agent program (Claude Code, Codex) instead of harness's own loop.
//!
//! A [`Provider`](crate::provider::Provider) whose [`delegated`](crate::provider::Provider::delegated)
//! answers `Some` makes the agent run a turn through [`DelegateTurns`]: the agent keeps what
//! belongs to the session (the user's message, the checkpoint, the confirmations, the record of
//! the child's session, the usage ledger, how the turn ends), and the implementation drives the
//! program, maps what it does onto [`AgentEvent`]s, and asks every approval through the
//! [`ApprovalGate`], which is harness's permission engine and approver, the only approver there
//! is for a delegated agent.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::ApprovalKind,
    event::{AgentEvent, TurnEndReason},
    message::Usage,
    meter::AccountKind,
    permission::{Action, Mode},
    redact::Redactor,
    role::{Role, SwitchReason},
};

/// One task for the external agent.
#[derive(Debug, Clone)]
pub struct DelegatedTurn {
    /// What the child is asked to do: the user's message, with the conversation so far summarised
    /// in front of it when the child's own session cannot be continued.
    pub task: String,
    /// The model's name for the program (without the provider's prefix).
    pub model: String,
    /// `<provider>/<model>`, as recorded.
    pub model_id: String,
    pub mode: Mode,
    /// Whether the user confirmed `full-access` for this session (the agent asked).
    pub full_access_confirmed: bool,
    pub workspace: PathBuf,
    /// The child's own session to continue, and the cost total it had reached.
    pub resume: Option<String>,
    pub cost_baseline_usd: f64,
    /// The project's instruction files, as harness loaded them.
    pub instructions: Option<String>,
    pub redactor: Arc<Redactor>,
    /// For the messages the turn shows: the role it runs for and why the model is not the
    /// role's own.
    pub role: Role,
    pub switch_reason: Option<SwitchReason>,
}

/// What a delegated turn did, for the agent to record.
#[derive(Debug, Clone, PartialEq)]
pub struct DelegatedOutcome {
    pub reason: TurnEndReason,
    /// The child's own id for its session, once it said it.
    pub session_id: Option<String>,
    /// The child's running cost total for the session, and what this turn added to it. Both
    /// are the child's estimate, never a bill.
    pub cost_total_usd: Option<f64>,
    pub estimated_usd: Option<f64>,
    /// What the child's requests used, summed.
    pub usage: Usage,
    /// The child's messages, in order.
    pub texts: Vec<String>,
    /// Tool calls the child made, and the files they changed.
    pub tool_calls: u32,
    /// The shell commands it ran, as it said them.
    pub commands: Vec<String>,
    pub changed: Vec<String>,
    /// Where the account's usage windows last stood, when the child said: the ledger's record of
    /// the turn names them.
    pub windows: Option<crate::meter::WindowSnapshot>,
}

impl DelegatedOutcome {
    /// A turn that ended as `reason` having done nothing.
    pub fn ended(reason: TurnEndReason) -> DelegatedOutcome {
        DelegatedOutcome {
            reason,
            session_id: None,
            cost_total_usd: None,
            estimated_usd: None,
            usage: Usage::default(),
            texts: Vec::new(),
            tool_calls: 0,
            commands: Vec::new(),
            changed: Vec::new(),
            windows: None,
        }
    }
}

/// What the child asks leave to do.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRequest {
    /// The child's id for the tool call.
    pub call_id: String,
    /// The name of the tool as shown to the user and to the rules (`bash`, `write`, `edit`, or the
    /// child's own name for a tool harness has no action for).
    pub tool: String,
    /// The call's arguments, **not redacted**: what the prompt shows of the change is worked out
    /// from them, and only what it shows is redacted. Never logged or saved.
    pub arguments: Value,
    /// What the call would do, in the permission engine's terms. Empty for a tool the engine has
    /// no action for: it is then always asked about, with `reason`.
    pub actions: Vec<Action>,
    /// The question, for a call with no action.
    pub reason: String,
}

/// What the engine and the user said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAnswer {
    /// Run it, this once.
    Allow,
    /// Not allowed, with what to tell the child.
    Deny { reason: String },
    /// Nobody can answer (a headless run): the action is blocked, and the run follows the exit
    /// rule of blocked actions.
    Unavailable { reason: String },
    /// The turn was stopped while waiting.
    Stopped,
}

/// harness's permission engine and approver, as a delegated agent sees them.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// The mode in force.
    fn mode(&self) -> Mode;

    /// Decides `request`: each action is checked by the engine (a refusal, then a question, the
    /// strictest wins), a rule's answer never reaches the user, and a question goes to the same
    /// approver as every other. An answer is for this one call: nothing is remembered as refused.
    async fn authorize(&self, request: GateRequest) -> GateAnswer;

    /// A plain yes-or-no question of `kind` (see [`ApprovalKind::is_question`]). `None` when
    /// nobody can answer it or the turn was stopped (see [`stopped`](Self::stopped)).
    async fn confirm(&self, kind: ApprovalKind, question: String) -> Option<bool>;

    /// Whether the user stopped the turn (Esc), which a `None` from [`confirm`](Self::confirm)
    /// may mean: an answer left undecided, to be asked again, not "nobody can answer".
    fn stopped(&self) -> bool {
        false
    }
}

/// An agent program driven for a whole turn.
#[async_trait]
pub trait DelegateTurns: Send + Sync {
    /// What pays for the turn: [`AccountKind::Delegated`] on a subscription, `ApiKey` where the
    /// program is run with the user's API key.
    fn account(&self) -> AccountKind;

    /// The name for messages: `claude-code`, `codex`.
    fn label(&self) -> String;

    /// Why this model cannot be used where nobody can be asked a question (a headless run), when
    /// that is already known: the opt-in is unanswered and there is no fallback. `None` when it
    /// can run, or can be asked.
    fn unusable_unasked(&self) -> Option<String> {
        None
    }

    /// Answers what has to be asked before the first turn (an opt-in), through `gate`. `Err`
    /// is why the model cannot be used: the turn ends with it.
    async fn prepare(&self, gate: &dyn ApprovalGate) -> Result<(), String>;

    /// Runs `turn`, sending what happens on `events` (everything but `TurnStarted` and
    /// `TurnFinished`, which the agent sends) and asking approvals through `gate`. `cancel`
    /// interrupts it; the outcome says how it ended.
    async fn run(
        &self,
        turn: DelegatedTurn,
        gate: &dyn ApprovalGate,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> DelegatedOutcome;
}
