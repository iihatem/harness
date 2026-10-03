//! Roles at work: a turn in `plan` mode runs on the `plan` role's model with that model's whole
//! profile, compaction runs on the `background` role, and a role whose model cannot be used ends
//! the turn without a request.

mod common;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use common::{roles::switches, *};
use futures::future::BoxFuture;
use harness_core::{
    agent::{Agent, NonInteractive},
    compaction::SUMMARY_SYSTEM,
    event::{AgentEvent, TurnEndReason},
    message::{ChatRequest, RequestOptions, Usage},
    meter::{
        AccountKind, Avoided, BudgetKind, BudgetNotice, BudgetStatus, Meter, RequestCost,
        RequestRecord,
    },
    permission::Mode,
    provider::{FinishReason, ProviderEvent},
    role::{ModelResolver, Role, RoleConfig, SwitchReason},
    session::Session,
    testing::{MockProvider, Script},
    turn::{TurnInput, TurnModel},
};
use tokio_util::sync::CancellationToken;

/// Resolves the ids it was given a model for.
struct Models(HashMap<String, TurnModel>);

impl ModelResolver for Models {
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

/// A model with a window and request options of its own.
fn model(id: &str, provider: &Arc<MockProvider>, window: u64) -> TurnModel {
    TurnModel {
        provider: provider.clone(),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        local: false,
        tools: None,
        edit_section: None,
        context_window: Some(window),
        request: Some(RequestOptions {
            max_output_tokens: Some(777),
            ..RequestOptions::default()
        }),
        text_tool_calls: false,
    }
}

fn resolver(models: Vec<TurnModel>) -> Arc<Models> {
    Arc::new(Models(
        models.into_iter().map(|m| (m.id.clone(), m)).collect(),
    ))
}

fn is_summary(request: &ChatRequest) -> bool {
    request.system == SUMMARY_SYSTEM && request.tools.is_empty()
}

fn planning(main: &Arc<MockProvider>, roles: RoleConfig, models: Vec<TurnModel>) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    agent(main.clone(), Mode::Plan, Arc::new(NonInteractive), &dir)
        .with_roles(roles)
        .with_resolver(resolver(models))
}

fn plan_role() -> RoleConfig {
    RoleConfig {
        plan: Some("plan/big".into()),
        ..RoleConfig::default()
    }
}

// Spec "Partial configuration": turns in `plan` mode run on the plan model, with its window and
// request options, and all other work on `main`.
#[tokio::test]
async fn a_plan_mode_turn_runs_on_the_plan_role_with_its_whole_profile() {
    let main = MockProvider::new(vec![Script::text("main answer")]);
    let plan = MockProvider::new(vec![Script::text("the plan")]);
    let mut agent = planning(&main, plan_role(), vec![model("plan/big", &plan, 200_000)]);
    let (reason, events) = run(&mut agent, "make a plan").await;
    assert_eq!(reason, TurnEndReason::Completed);
    let request = plan.requests().pop().unwrap();
    assert_eq!(request.model, "big");
    assert_eq!(request.options.max_output_tokens, Some(777));
    let room = request
        .output_room
        .expect("the plan model's window is known");
    assert!(room > 190_000 && room < 200_000, "{room}");
    assert!(main.requests().is_empty());
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::AssistantMessage { model, .. } if model == "plan/big"
        )),
        "{events:?}"
    );
    // Out of plan mode, the next turn is main's again.
    agent.set_mode(Mode::Auto);
    run(&mut agent, "now do it").await;
    assert_eq!(main.requests().len(), 1);
    assert_eq!(plan.requests().len(), 1);
    assert_eq!(main.requests()[0].model, "m1");
}

// Spec "Defaults": no roles set, so planning runs on the session's model.
#[tokio::test]
async fn without_roles_a_plan_mode_turn_runs_on_main() {
    let main = MockProvider::new(vec![Script::text("a plan")]);
    let mut agent = planning(&main, RoleConfig::default(), vec![]);
    let (reason, _) = run(&mut agent, "make a plan").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(main.requests().len(), 1);
}

// A role set to the model main already is needs no second provider.
#[tokio::test]
async fn a_role_that_names_main_runs_on_main() {
    let main = MockProvider::new(vec![Script::text("a plan")]);
    let roles = RoleConfig {
        plan: Some("mock/m1".into()),
        ..RoleConfig::default()
    };
    let mut agent = planning(&main, roles, vec![]);
    let (reason, _) = run(&mut agent, "make a plan").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(main.requests().len(), 1);
}

#[tokio::test]
async fn a_role_whose_model_cannot_be_used_ends_the_turn_without_a_request() {
    let main = MockProvider::new(vec![Script::text("never")]);
    let mut agent = planning(&main, plan_role(), vec![]);
    let (reason, events) = run(&mut agent, "make a plan").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(main.requests().is_empty());
    let message = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("an error event");
    assert!(
        message.contains("plan role")
            && message.contains("plan/big has no credentials")
            && message.contains("/model --role plan")
            // `harness ask` has no slash commands: the configuration is the other way.
            && message.contains("[roles].plan in config.toml"),
        "{message}"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnFinished {
            reason: TurnEndReason::Error
        })
    ));
}

// A slash command that names a model asks for it on purpose: it goes over the role's.
#[tokio::test]
async fn a_commands_model_goes_over_the_role() {
    let main = MockProvider::new(vec![]);
    let plan = MockProvider::new(vec![]);
    let command = MockProvider::new(vec![Script::text("from the command")]);
    let mut agent = planning(&main, plan_role(), vec![model("plan/big", &plan, 200_000)]);
    let input = TurnInput {
        model: Some(model("cmd/x", &command, 50_000)),
        ..TurnInput::from("go")
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn(input, &tx, CancellationToken::new()).await;
    assert_eq!(command.requests().len(), 1);
    assert!(plan.requests().is_empty() && main.requests().is_empty());
}

// A command file's `model:` is a switch of the user's own: it is announced with the reason
// `user`, for `ask --json` and any other frontend, and the way back to main is announced too.
#[tokio::test]
async fn a_commands_model_is_announced_as_a_user_switch() {
    let main = MockProvider::new(vec![Script::text("main again")]);
    let command = MockProvider::new(vec![Script::text("from the command")]);
    let mut agent = planning(&main, RoleConfig::default(), vec![]);
    let input = TurnInput {
        model: Some(model("cmd/x", &command, 50_000)),
        ..TurnInput::from("go")
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn(input, &tx, CancellationToken::new()).await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(
        switches(&events),
        [(
            "mock/m1".to_string(),
            "cmd/x".to_string(),
            Role::Plan,
            SwitchReason::User
        )]
    );
    // The next turn, on main again, is announced as the way back.
    let (_, events) = run(&mut agent, "and now").await;
    assert_eq!(
        switches(&events),
        [(
            "cmd/x".to_string(),
            "mock/m1".to_string(),
            Role::Plan,
            SwitchReason::User
        )]
    );
}

// `/model --role plan <id>`: the session's own setting is used from the next turn.
#[tokio::test]
async fn a_role_set_for_the_session_is_used_at_the_next_turn() {
    let main = MockProvider::new(vec![]);
    let first = MockProvider::new(vec![Script::text("one")]);
    let second = MockProvider::new(vec![Script::text("two")]);
    let mut agent = planning(
        &main,
        plan_role(),
        vec![
            model("plan/big", &first, 200_000),
            model("plan/other", &second, 100_000),
        ],
    );
    run(&mut agent, "one").await;
    assert!(agent.set_role_model(Role::Plan, "plan/other"));
    run(&mut agent, "two").await;
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), 1);
    assert!(!agent.set_role_model(Role::Main, "plan/other"));
}

/// Records every request and how it was counted.
#[derive(Default)]
struct Recording {
    records: Mutex<Vec<RequestRecord>>,
    /// Refuse requests on an API key.
    stop: std::sync::atomic::AtomicBool,
}

impl Meter for Recording {
    fn record_request(&self, request: &RequestRecord) -> RequestCost {
        self.records.lock().unwrap().push(request.clone());
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.1),
            list_usd: Some(0.1),
            avoided: Avoided::NotApplicable,
        }
    }

    fn check_budget(&self, _session: &str, account: AccountKind) -> BudgetStatus {
        if self.stop.load(std::sync::atomic::Ordering::SeqCst) && account == AccountKind::ApiKey {
            return BudgetStatus {
                stop: Some(BudgetNotice {
                    budget: BudgetKind::Daily,
                    spent_usd: 5.0,
                    limit_usd: 5.0,
                }),
                ..BudgetStatus::default()
            };
        }
        BudgetStatus::default()
    }
}

/// An agent on `main` after one turn with a 2,000-token message, saved in a session, set up to
/// compact at the next request: its window is 2,500 tokens.
async fn after_a_long_turn(
    main: &Arc<MockProvider>,
    roles: RoleConfig,
    models: Vec<TurnModel>,
    meter: Option<Arc<Recording>>,
) -> Agent {
    let dir = tempfile::tempdir().unwrap().keep();
    let mut agent = agent(main.clone(), Mode::Auto, Arc::new(NonInteractive), &dir)
        .with_session(Session::create(&dir.join("sessions"), &dir))
        .with_roles(roles)
        .with_resolver(resolver(models));
    if let Some(meter) = meter {
        agent = agent.with_meter(meter);
    }
    run(&mut agent, &"x".repeat(8_000)).await;
    agent.config_mut().context_window = 2_500;
    agent
}

// Spec "Compaction on the background role": the summary request goes to `ollama/llama3` (here
// `bg/small`) while `main` goes on answering; the request is metered under that role and model.
#[tokio::test]
async fn compaction_runs_on_the_background_role() {
    let main = MockProvider::new(vec![Script::text("noted"), Script::text("answer")]);
    let background = MockProvider::new(vec![Script::text("They pasted 8,000 x's.")]);
    let roles = RoleConfig {
        background: Some("bg/small".into()),
        ..RoleConfig::default()
    };
    let meter = Arc::new(Recording::default());
    let mut agent = after_a_long_turn(
        &main,
        roles,
        vec![model("bg/small", &background, 100_000)],
        Some(meter.clone()),
    )
    .await;
    let (reason, events) = run(&mut agent, "short question").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. }))
    );
    let summaries = background.requests();
    assert_eq!(summaries.len(), 1);
    assert!(is_summary(&summaries[0]));
    assert_eq!(summaries[0].model, "small");
    assert_eq!(summaries[0].options.max_output_tokens, Some(777));
    // Main answered the two turns and wrote nothing.
    assert_eq!(main.requests().len(), 2);
    assert!(main.requests().iter().all(|r| !is_summary(r)));
    let records = meter.records.lock().unwrap();
    let summary = records.iter().find(|r| r.model == "bg/small").unwrap();
    assert_eq!(summary.role, "background");
    assert!(
        records
            .iter()
            .filter(|r| r.model == "mock/m1")
            .all(|r| r.role == "main")
    );
}

// Spec "Unset background role": compaction runs on `main`, even in a turn on another role's
// model, whose window still decides when to compact.
#[tokio::test]
async fn an_unset_background_role_compacts_on_main_in_a_plan_turn() {
    let main = MockProvider::new(vec![
        Script::text("noted"),
        Script::text("They pasted 8,000 x's."),
    ]);
    let plan = MockProvider::new(vec![Script::text("a plan")]);
    let mut agent = after_a_long_turn(
        &main,
        plan_role(),
        vec![model("plan/big", &plan, 2_500)],
        None,
    )
    .await;
    agent.set_mode(Mode::Plan);
    let (reason, events) = run(&mut agent, "short question").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. }))
    );
    assert!(is_summary(main.requests().last().unwrap()));
    assert_eq!(plan.requests().len(), 1);
    assert!(!is_summary(&plan.requests()[0]));
}

#[tokio::test]
async fn a_background_model_that_cannot_be_used_leaves_the_conversation_as_it_is() {
    let main = MockProvider::new(vec![Script::text("noted"), Script::text("answer")]);
    let roles = RoleConfig {
        background: Some("bg/small".into()),
        ..RoleConfig::default()
    };
    let mut agent = after_a_long_turn(&main, roles, vec![], None).await;
    let (reason, events) = run(&mut agent, "short question").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. }))
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning { message }
                if message.contains("could not compact") && message.contains("background")
        )),
        "{events:?}"
    );
}

// D3: an automatic compaction is a request like any other, so a billed background model is not
// asked once the budget is reached, even when the turn's own model is local.
#[tokio::test]
async fn a_billed_background_model_is_not_asked_once_a_budget_is_reached() {
    let main = MockProvider::new(vec![Script::text("noted"), Script::text("answer")]);
    let background = MockProvider::new(vec![Script::text("summary")]);
    let roles = RoleConfig {
        background: Some("bg/small".into()),
        ..RoleConfig::default()
    };
    let meter = Arc::new(Recording::default());
    let mut agent = after_a_long_turn(
        &main,
        roles,
        vec![model("bg/small", &background, 100_000)],
        Some(meter.clone()),
    )
    .await;
    // The budget is reached after the first turn.
    meter.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    // The session's own model is local, which a reached budget does not stop.
    agent.config_mut().request.local = true;
    let (reason, events) = run(&mut agent, "short question").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(background.requests().is_empty());
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning { message }
                if message.contains("daily budget") && message.contains("compact")
        )),
        "{events:?}"
    );
}

// Spec "Background window too small": a background model whose window cannot hold what is to be
// summarized is not asked; a warning says so and the conversation is as it was. The oldest part
// is never silently left out of a summary.
#[tokio::test]
async fn a_background_window_smaller_than_the_part_to_summarize_fails_with_a_warning() {
    let main = MockProvider::new(vec![Script::text("noted"), Script::text("answer")]);
    let background = MockProvider::new(vec![Script::text("a summary of the tail")]);
    let roles = RoleConfig {
        background: Some("bg/small".into()),
        ..RoleConfig::default()
    };
    let mut agent = after_a_long_turn(
        &main,
        roles,
        // The part to summarize is about 2,000 tokens.
        vec![model("bg/small", &background, 1_000)],
        None,
    )
    .await;
    let before = agent.history().to_vec();
    let (_, events) = run(&mut agent, "short question").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning { message }
                if message.contains("could not compact") && message.contains("window")
        )),
        "{events:?}"
    );
    assert!(background.requests().is_empty());
    // The conversation is as it was, plus the new turn.
    assert_eq!(&agent.history()[..before.len()], &before[..]);
}

// The tokens the last request reported are counted by the model that answered it: a turn on
// another model estimates its input afresh instead of trusting them.
#[tokio::test]
async fn a_turn_on_another_model_does_not_trust_the_last_models_token_counts() {
    let main = MockProvider::new(vec![
        Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("noted".into())),
            Ok(ProviderEvent::Usage(Usage {
                input_tokens: 50_000,
                ..Default::default()
            })),
            Ok(ProviderEvent::Finished(FinishReason::Stop)),
        ]),
        Script::text("never asked"),
    ]);
    let plan = MockProvider::new(vec![Script::text("a plan")]);
    let mut agent = planning(&main, plan_role(), vec![model("plan/big", &plan, 60_000)]);
    agent.set_mode(Mode::Auto);
    run(&mut agent, "hello").await;
    agent.set_mode(Mode::Plan);
    let (reason, events) = run(&mut agent, "plan it").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. } | AgentEvent::Warning { .. })),
        "{events:?}"
    );
    assert_eq!(plan.requests().len(), 1);
    assert_eq!(main.requests().len(), 1);
}

/// A meter that always says a budget is paused, which stops nothing.
struct Paused;

impl Meter for Paused {
    fn record_request(&self, _request: &RequestRecord) -> RequestCost {
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.0),
            list_usd: Some(0.0),
            avoided: Avoided::NotApplicable,
        }
    }

    fn check_budget(&self, _session: &str, _account: AccountKind) -> BudgetStatus {
        BudgetStatus {
            paused: Some(BudgetNotice {
                budget: BudgetKind::Daily,
                spent_usd: 5.0,
                limit_usd: 5.0,
            }),
            ..BudgetStatus::default()
        }
    }
}

// A paused budget is said once per turn, however many times it is checked: before the request,
// and again for a compaction.
#[tokio::test]
async fn a_paused_budget_is_said_once_in_a_turn_that_also_compacts() {
    let main = MockProvider::new(vec![
        Script::text("noted"),
        Script::text("summary"),
        Script::text("answer"),
    ]);
    let mut agent = after_a_long_turn(&main, RoleConfig::default(), vec![], None).await;
    agent = agent.with_meter(Arc::new(Paused));
    let (_, events) = run(&mut agent, "short question").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })),
        "{events:?}"
    );
    let said = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Warning { message } if message.contains("paused")))
        .count();
    assert_eq!(said, 1, "{events:?}");
}
