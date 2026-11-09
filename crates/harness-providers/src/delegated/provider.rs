//! The provider for `claude-code/*` and `codex/*`, and the loop that turns a child's events into
//! the agent's.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{self, FuturesUnordered},
};
use harness_core::{
    agent::ApprovalKind,
    delegated::{ApprovalGate, DelegateTurns, DelegatedOutcome, DelegatedTurn, GateAnswer},
    event::{AgentEvent, ErrorKind, TurnEndReason},
    message::{ChatRequest, Usage},
    meter::{AccountKind, WindowSnapshot},
    provider::{Provider, ProviderError, ProviderStream},
};
use harness_delegate::{
    BackendKind, ClaudeBackend, CodexBackend, DelegateBackend, StartRequest,
    env::{ApiKey, AuthMode},
    event::{ApprovalAnswer, DelegateEvent, LimitStatus},
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{
    CLAUDE_CODE, CODEX, DelegateSettings,
    approve::{self, Step},
    optin::{self, Auth},
};

struct Inner {
    kind: BackendKind,
    model: String,
    backend: Arc<dyn DelegateBackend>,
    settings: DelegateSettings,
    auth: Mutex<Auth>,
    windows: Mutex<Option<WindowSnapshot>>,
    said: AtomicBool,
    /// The question about the subscription could not be put to anybody, so the key was used.
    unasked: AtomicBool,
    /// Why the answer to the question could not be kept, said at the next run.
    unkept: Mutex<Option<String>>,
    /// Held while the opt-in is asked: children running at once ask it once, the others take the
    /// answer.
    asking: tokio::sync::Mutex<()>,
}

/// A model that runs in `claude` or `codex`.
#[derive(Clone)]
pub struct DelegatedProvider {
    inner: Arc<Inner>,
}

impl DelegatedProvider {
    /// The provider for `<provider>/<model>`: `claude-code` with the opt-in `settings` hold and
    /// `key` (the user's Anthropic API key, when there is one), or `codex`.
    pub fn new(
        provider: &str,
        model: &str,
        settings: DelegateSettings,
        key: Option<ApiKey>,
    ) -> Option<DelegatedProvider> {
        let (kind, auth, backend): (_, _, Arc<dyn DelegateBackend>) = match provider {
            CLAUDE_CODE => (
                BackendKind::Claude,
                optin::claude_auth(settings.claude_subscription, key),
                Arc::new(ClaudeBackend),
            ),
            CODEX => (BackendKind::Codex, Auth::Own, Arc::new(CodexBackend)),
            _ => return None,
        };
        Some(DelegatedProvider::with_backend(
            kind, model, settings, auth, backend,
        ))
    }

    /// With another backend: a test's.
    pub fn with_backend(
        kind: BackendKind,
        model: &str,
        settings: DelegateSettings,
        auth: Auth,
        backend: Arc<dyn DelegateBackend>,
    ) -> DelegatedProvider {
        DelegatedProvider {
            inner: Arc::new(Inner {
                kind,
                model: model.to_string(),
                backend,
                settings,
                auth: Mutex::new(auth),
                windows: Mutex::new(None),
                said: AtomicBool::new(false),
                unasked: AtomicBool::new(false),
                unkept: Mutex::new(None),
                asking: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Why the model cannot be used with the settings it was made with, when that is known now.
    pub fn unusable(&self) -> Option<String> {
        match &*self.inner.auth.lock().expect("auth lock") {
            Auth::Unusable(why) => Some(why.clone()),
            _ => None,
        }
    }
}

impl Provider for DelegatedProvider {
    fn stream(&self, _request: ChatRequest) -> ProviderStream {
        // A turn on this model goes through `delegated`; this is never asked.
        Box::pin(stream::once(async {
            Err(ProviderError::InStream(
                "this model runs in another program and takes no requests".into(),
            ))
        }))
    }

    fn windows(&self) -> Option<BoxFuture<'static, Result<WindowSnapshot, String>>> {
        // Claude reports no usage percentages: there is nothing to wait for.
        if self.inner.kind == BackendKind::Claude {
            return None;
        }
        let seen = self.inner.windows.lock().expect("windows lock").clone();
        Some(
            async move {
                seen.ok_or_else(|| {
                    "no usage windows were reported yet: they come with the first turn".to_string()
                })
            }
            .boxed(),
        )
    }

    fn delegated(&self) -> Option<Arc<dyn DelegateTurns>> {
        Some(self.inner.clone())
    }
}

impl Inner {
    fn auth_mode(&self) -> Result<AuthMode, String> {
        match &*self.auth.lock().expect("auth lock") {
            Auth::Own => Ok(AuthMode::Subscription),
            Auth::Key(key) => Ok(AuthMode::ApiKey(key.clone())),
            Auth::Undecided { .. } => Err("the account was not chosen".into()),
            Auth::Unusable(why) => Err(why.clone()),
        }
    }

    /// What is said once, the first time a model runs: which account pays.
    fn say_account(&self, events: &UnboundedSender<AgentEvent>) {
        if self.said.swap(true, Ordering::Relaxed) || self.kind != BackendKind::Claude {
            return;
        }
        let message = match &*self.auth.lock().expect("auth lock") {
            Auth::Key(_) if self.unasked.load(Ordering::Relaxed) => {
                "nobody could be asked whether claude-code may use your Claude subscription, so it runs on your Anthropic API key, billed per token and counted by the money budgets (nothing was stored; set [delegate.claude] use_subscription = true in the global config.toml to use the subscription)"
            }
            Auth::Key(_) => {
                "claude-code runs on your Anthropic API key, billed per token and counted by the money budgets; to use your Claude subscription instead, set [delegate.claude] use_subscription = true in the global config.toml"
            }
            Auth::Own => {
                "claude-code runs on your Claude subscription through the claude program you signed in to; its cost figures are the program's estimates, never a bill"
            }
            _ => return,
        };
        let _ = events.send(AgentEvent::Warning {
            message: message.into(),
        });
    }
}

fn answer_for(answer: GateAnswer) -> Option<ApprovalAnswer> {
    match answer {
        GateAnswer::Allow => Some(ApprovalAnswer::AllowOnce),
        GateAnswer::Deny { reason } | GateAnswer::Unavailable { reason } => {
            Some(ApprovalAnswer::Deny {
                reason: Some(reason),
            })
        }
        GateAnswer::Stopped => Some(ApprovalAnswer::Cancel),
    }
}

#[async_trait]
impl DelegateTurns for Inner {
    fn account(&self) -> AccountKind {
        match &*self.auth.lock().expect("auth lock") {
            Auth::Key(_) => AccountKind::ApiKey,
            _ => AccountKind::Delegated,
        }
    }

    fn label(&self) -> String {
        match self.kind {
            BackendKind::Claude => CLAUDE_CODE,
            BackendKind::Codex => CODEX,
        }
        .to_string()
    }

    fn unusable_unasked(&self) -> Option<String> {
        // An answer given since this model was made decides, as it does at the first turn.
        let answered = self.settings.store.as_ref().and_then(|store| store.get());
        match &*self.auth.lock().expect("auth lock") {
            Auth::Undecided { key: None } if answered.is_none() => {
                Some(optin::NO_ACCOUNT.to_string())
            }
            Auth::Undecided { key: None } if answered == Some(false) => {
                Some(optin::REFUSED_NO_KEY.to_string())
            }
            _ => None,
        }
    }

    async fn prepare(&self, gate: &dyn ApprovalGate) -> Result<(), String> {
        let _asking = self.asking.lock().await;
        // An answer given since this model was made (by another child) decides.
        if let Some(yes) = self.settings.store.as_ref().and_then(|store| store.get()) {
            let mut auth = self.auth.lock().expect("auth lock");
            if let Auth::Undecided { key } = &*auth {
                *auth = optin::claude_auth(Some(yes), key.clone());
            }
        }
        let key = match &*self.auth.lock().expect("auth lock") {
            Auth::Undecided { key } => key.clone(),
            Auth::Unusable(why) => return Err(why.clone()),
            _ => return Ok(()),
        };
        let answer = gate
            .confirm(ApprovalKind::DelegateOptIn, optin::DISCLOSURE.to_string())
            .await;
        let mut auth = self.auth.lock().expect("auth lock");
        match answer {
            Some(yes) => {
                if let Some(store) = &self.settings.store {
                    // An answer that cannot be kept is still used for this session, and the
                    // run says it was not kept.
                    if let Err(why) = store.set(yes) {
                        *self.unkept.lock().expect("unkept lock") = Some(why);
                    }
                }
                *auth = optin::claude_auth(Some(yes), key);
            }
            // Esc: the user answered nothing and the turn ends. The question stays open, and the
            // next turn asks it again; nothing is stored and no account is chosen.
            None if gate.stopped() => {
                return Err("the question about your Claude subscription was left unanswered: it is asked again at the next turn".into());
            }
            // Nobody can answer (a headless run): not stored, asked again next time, and for now
            // the API key if there is one, which is said.
            None => {
                *auth = match key {
                    Some(key) => Auth::Key(key),
                    None => return Err(optin::NO_ACCOUNT.to_string()),
                };
                self.unasked.store(true, Ordering::Relaxed);
            }
        }
        match &*auth {
            Auth::Unusable(why) => Err(why.clone()),
            _ => Ok(()),
        }
    }

    async fn run(
        &self,
        turn: DelegatedTurn,
        gate: &dyn ApprovalGate,
        events: &UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> DelegatedOutcome {
        let fail = |message: String| {
            let _ = events.send(AgentEvent::Error {
                kind: ErrorKind::Provider,
                message,
            });
            DelegatedOutcome::ended(TurnEndReason::Error)
        };
        let auth = match self.auth_mode() {
            Ok(auth) => auth,
            Err(why) => return fail(why),
        };
        self.say_account(events);
        if let Some(why) = self.unkept.lock().expect("unkept lock").take() {
            let _ = events.send(AgentEvent::Warning {
                message: format!(
                    "your answer about using your Claude subscription could not be kept ({why}): it applies to this session only, and you are asked again next time"
                ),
            });
        }
        let mut request = StartRequest::new(turn.task.clone(), turn.workspace.clone());
        request.model = Some(self.model.clone());
        request.mode = turn.mode;
        request.full_access_confirmed = turn.full_access_confirmed;
        request.resume = turn.resume.clone();
        request.auth = auth;
        request.redactor = turn.redactor.clone();
        request.allow_untested = self.settings.allow_untested;
        request.cost_baseline_usd = turn.cost_baseline_usd;
        request.instructions = turn.instructions.clone();
        request.binary = match self.kind {
            BackendKind::Claude => self.settings.claude_binary.clone(),
            BackendKind::Codex => self.settings.codex_binary.clone(),
        };
        let mut run = match self.backend.start(request).await {
            Ok(run) => run,
            Err(e) => return fail(format!("{} cannot be used: {e}", self.label())),
        };

        let mut outcome = DelegatedOutcome::ended(TurnEndReason::Error);
        // Text streamed and not yet closed into a message.
        let mut streamed = String::new();
        let mut pending: FuturesUnordered<BoxFuture<'_, (String, Option<GateAnswer>)>> =
            FuturesUnordered::new();
        let mut withdrawn: HashMap<String, CancellationToken> = HashMap::new();
        let mut interrupted = false;
        let mut said_limit = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if !interrupted => {
                    interrupted = true;
                    let _ = run.control.interrupt();
                }
                Some((id, answer)) = pending.next(), if !pending.is_empty() => {
                    withdrawn.remove(&id);
                    // A withdrawn approval is not answered: the child no longer waits.
                    if let Some(answer) = answer.and_then(answer_for) {
                        let _ = run.control.answer(&id, answer).await;
                    }
                }
                event = run.events.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        DelegateEvent::Started { session_id, .. } => {
                            outcome.session_id = Some(session_id);
                        }
                        DelegateEvent::TextDelta { text } => {
                            streamed.push_str(&text);
                            let _ = events.send(AgentEvent::TextDelta { text });
                        }
                        DelegateEvent::ReasoningDelta { text } => {
                            let _ = events.send(AgentEvent::ReasoningDelta { text });
                        }
                        DelegateEvent::AssistantMessage { content, .. } => {
                            streamed.clear();
                            let _ = events.send(AgentEvent::AssistantMessage {
                                content: content.clone(),
                                model: turn.model_id.clone(),
                                role: turn.role,
                                switch_reason: turn.switch_reason,
                            });
                            outcome.texts.push(content);
                        }
                        DelegateEvent::ToolCallRequested { id, name, arguments } => {
                            outcome.tool_calls += 1;
                            if let Some(command) = shell_command(&name, &arguments) {
                                outcome.commands.push(command);
                            }
                            let _ = events.send(AgentEvent::ToolCallRequested { id, name, arguments });
                        }
                        DelegateEvent::ApprovalNeeded(approval) => {
                            match approve::map(self.kind, &approval, &turn.workspace, gate.mode()) {
                                Step::Refuse(reason) => {
                                    let _ = run
                                        .control
                                        .answer(&approval.id, ApprovalAnswer::Deny { reason: Some(reason) })
                                        .await;
                                }
                                Step::Ask(request) => {
                                    let id = approval.id.clone();
                                    let token = CancellationToken::new();
                                    withdrawn.insert(id.clone(), token.clone());
                                    pending.push(
                                        async move {
                                            tokio::select! {
                                                biased;
                                                _ = token.cancelled() => (id, None),
                                                answer = gate.authorize(request) => (id, Some(answer)),
                                            }
                                        }
                                        .boxed(),
                                    );
                                }
                            }
                        }
                        DelegateEvent::ApprovalWithdrawn { id } => {
                            if let Some(token) = withdrawn.get(&id) {
                                token.cancel();
                            }
                        }
                        DelegateEvent::ToolCallFinished { id, output, is_error } => {
                            let _ = events.send(AgentEvent::ToolCallFinished { id, output, is_error });
                        }
                        DelegateEvent::FileChanged { path, .. } => {
                            if !outcome.changed.contains(&path) {
                                outcome.changed.push(path);
                            }
                        }
                        DelegateEvent::Usage { usage, .. } => {
                            add(&mut outcome.usage, &usage);
                            let _ = events.send(AgentEvent::Usage {
                                model: turn.model_id.clone(),
                                usage,
                            });
                        }
                        DelegateEvent::Cost { estimated_usd, session_total_usd } => {
                            outcome.estimated_usd =
                                Some(outcome.estimated_usd.unwrap_or(0.0) + estimated_usd);
                            outcome.cost_total_usd = Some(session_total_usd);
                        }
                        DelegateEvent::RateLimits { snapshot, status, .. } => {
                            if !snapshot.windows.is_empty() {
                                *self.windows.lock().expect("windows lock") =
                                    Some(snapshot.clone());
                                outcome.windows = Some(snapshot.clone());
                                let _ = events.send(AgentEvent::RateLimits { snapshot });
                            }
                            let said = match status {
                                Some(LimitStatus::Warning) => Some("claude-code says the subscription's usage limit is near"),
                                Some(LimitStatus::Rejected) => Some("claude-code says the subscription's usage limit is reached: requests are refused until it resets"),
                                _ => None,
                            };
                            if let Some(message) = said
                                && !std::mem::replace(&mut said_limit, true)
                            {
                                let _ = events.send(AgentEvent::Warning { message: message.into() });
                            }
                        }
                        DelegateEvent::Retrying { attempt, reason, delay_ms } => {
                            let _ = events.send(AgentEvent::Retrying { attempt, reason, delay_ms });
                        }
                        DelegateEvent::Warning { message } => {
                            let _ = events.send(AgentEvent::Warning { message });
                        }
                        DelegateEvent::Error { kind, message } => {
                            let _ = events.send(AgentEvent::Error { kind, message });
                        }
                        DelegateEvent::Finished { reason, .. } => {
                            outcome.reason = reason;
                        }
                    }
                }
            }
        }
        // Text the child streamed but never closed into a message (an interrupted reply) still
        // counts as what it said.
        if !streamed.trim().is_empty() {
            outcome.texts.push(streamed);
        }
        if outcome.session_id.is_none() {
            outcome.session_id = run.control.session_id();
        }
        outcome
    }
}

/// The command of a shell call of the program (Claude's `Bash`, Codex's `command`).
fn shell_command(name: &str, arguments: &str) -> Option<String> {
    if !matches!(name, "Bash" | "command" | "bash") {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()?
        .get("command")?
        .as_str()
        .map(String::from)
}

fn add(total: &mut Usage, usage: &Usage) {
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.cached_tokens += usage.cached_tokens;
    total.cache_write_tokens += usage.cache_write_tokens;
    total.cache_write_1h_tokens += usage.cache_write_1h_tokens;
    total.reasoning_tokens += usage.reasoning_tokens;
}
