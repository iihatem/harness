//! A request that fails can be sent again on the next usable model of a configured chain, once
//! per turn, announced, and never for the failures a chain cannot help with.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{roles::*, *};
use harness_core::{
    agent::{Agent, NonInteractive},
    event::{AgentEvent, TurnEndReason},
    meter::{
        AccountKind, Avoided, BudgetKind, BudgetNotice, BudgetStatus, Meter, RequestCost,
        RequestRecord,
    },
    permission::Mode,
    provider::ProviderError,
    retry::RetryPolicy,
    role::{Role, SwitchReason},
    session::Session,
    testing::{MockProvider, Script},
    turn::TurnModel,
};

const QUOTA: &str = r#"{"error":{"type":"usage_limit_reached","resets_at":1893456000}}"#;

fn http(status: u16, body: &str) -> ProviderError {
    ProviderError::Http {
        status,
        body: body.into(),
        retry_after: None,
    }
}

fn quota() -> Script {
    Script::error(http(429, QUOTA))
}

/// `n` copies of the failure `make` builds: what five attempts meet.
fn times(n: usize, make: impl Fn() -> ProviderError) -> Vec<Script> {
    (0..n).map(|_| Script::error(make())).collect()
}

fn fast_retries(agent: &mut Agent) {
    agent.config_mut().retry = RetryPolicy {
        max_attempts: 5,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    };
}

struct Candidate {
    id: &'static str,
    provider: Arc<MockProvider>,
    window: u64,
    local: bool,
}

fn candidate(id: &'static str, script: Vec<Script>) -> Candidate {
    Candidate {
        id,
        provider: MockProvider::new(script),
        window: 500_000,
        local: false,
    }
}

/// An agent on `mock/m1` (400,000 tokens of window) whose primary provider answers `script`, with
/// the chain `chain` over `candidates`.
fn setup(
    script: Vec<Script>,
    candidates: &[&Candidate],
    chain: &[&str],
) -> (Agent, Arc<MockProvider>) {
    let dir = tempfile::tempdir().unwrap().keep();
    let primary = MockProvider::new(script);
    let models = candidates
        .iter()
        .map(|c| TurnModel {
            local: c.local,
            request: Some(harness_core::message::RequestOptions {
                local: c.local,
                ..Default::default()
            }),
            ..model(c.id, &c.provider, c.window)
        })
        .collect();
    let mut agent = with_chain(
        agent(primary.clone(), Mode::Auto, Arc::new(NonInteractive), &dir),
        models,
        &[("mock/m1", chain), ("chatgpt/m1", chain)],
    )
    .with_session(Session::create(&dir.join("sessions"), &dir));
    agent.config_mut().context_window = 400_000;
    fast_retries(&mut agent);
    (agent, primary)
}

fn errors(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Error { .. }))
        .count()
}

fn detail_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ModelSwitched { detail, .. } => detail.clone(),
            _ => None,
        })
        .unwrap_or_default()
}

// Spec "Subscription limit with a chain": the window is exhausted, the chain is `["openai/gpt-5"]`
// with credentials. A `ModelSwitched` event with reason `fallback` names it as billed, the request
// is re-sent there, and the next turn uses the primary model again.
#[tokio::test]
async fn a_subscription_limit_falls_back_and_the_next_turn_returns() {
    let fallback = candidate("openai/gpt-5", vec![Script::text("from the fallback")]);
    let (mut agent, primary) = setup(
        vec![quota(), Script::text("primary again")],
        &[&fallback],
        &["openai/gpt-5"],
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(errors(&events), 0, "{events:?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::LimitReached { .. }))
    );
    assert_eq!(
        switches(&events),
        [(
            "mock/m1".into(),
            "openai/gpt-5".into(),
            Role::Main,
            SwitchReason::Fallback
        )]
    );
    let detail = detail_of(&events);
    assert!(
        detail.contains("usage limit") && detail.contains("openai/gpt-5 is billed"),
        "{detail}"
    );
    // The same request, re-sent.
    let sent = fallback.provider.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].model, "gpt-5");
    assert_eq!(sent[0].messages, primary.requests()[0].messages);
    // Spec "Message attribution after a fallback".
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AssistantMessage { model, role: Role::Main, switch_reason: Some(SwitchReason::Fallback), .. }
            if model == "openai/gpt-5"
    )));
    let (reason, events) = run(&mut agent, "next").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert!(
        switches(&events).is_empty(),
        "no announcement of the return"
    );
    assert_eq!(primary.requests().len(), 2);
    assert_eq!(fallback.provider.requests().len(), 1);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AssistantMessage { model, switch_reason: None, .. } if model == "mock/m1"
    )));
}

// Spec "Chain candidate skipped" and "Smaller window skipped".
#[tokio::test]
async fn a_candidate_on_the_other_side_or_with_a_smaller_window_is_skipped() {
    let local = Candidate {
        local: true,
        ..candidate("ollama/llama3", vec![Script::text("never")])
    };
    let small = Candidate {
        window: 128_000,
        ..candidate("small/model", vec![Script::text("never")])
    };
    let good = candidate("openai/gpt-5", vec![Script::text("from the good one")]);
    let (mut agent, _) = setup(
        vec![quota()],
        &[&local, &small, &good],
        &["ollama/llama3", "small/model", "openai/gpt-5"],
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(switches(&events)[0].1, "openai/gpt-5");
    assert!(local.provider.requests().is_empty() && small.provider.requests().is_empty());
}

#[tokio::test]
async fn a_local_model_falls_back_only_to_local_models() {
    let hosted = candidate("openai/gpt-5", vec![Script::text("never")]);
    let local = Candidate {
        local: true,
        ..candidate("ollama/llama3", vec![Script::text("from the local one")])
    };
    let (mut agent, _) = setup(
        times(5, || http(503, "down")),
        &[&hosted, &local],
        &["openai/gpt-5", "ollama/llama3"],
    );
    agent.config_mut().request.local = true;
    // Five attempts fail it first.
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    assert_eq!(switches(&events)[0].1, "ollama/llama3");
    assert!(
        detail_of(&events).contains("not billed"),
        "{}",
        detail_of(&events)
    );
    assert!(hosted.provider.requests().is_empty());
}

// Spec "Second failure": the fallback model fails too, so the turn ends as without a chain.
#[tokio::test]
async fn a_second_failure_in_the_turn_ends_it() {
    let first = candidate("fb/one", vec![quota()]);
    let second = candidate("fb/two", vec![Script::text("never asked")]);
    let (mut agent, _) = setup(vec![quota()], &[&first, &second], &["fb/one", "fb/two"]);
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(switches(&events).len(), 1);
    assert_eq!(errors(&events), 1);
    assert!(second.provider.requests().is_empty());
}

// Spec "No chain": the turn stops as before, and the limit's reset time is told for the
// auto-resume offer.
#[tokio::test]
async fn without_a_chain_the_turn_stops_and_the_limit_is_reported() {
    let (mut agent, _) = setup(vec![quota()], &[], &[]);
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::LimitReached {
            resets_at: 1_893_456_000
        }
    )));
    assert!(switches(&events).is_empty());
}

// A chain none of whose models can be used is as no chain, and says why.
#[tokio::test]
async fn a_chain_with_no_usable_model_says_why_and_stops_as_before() {
    // `ghost/model` has no credentials: the resolver does not have it.
    let (mut agent, _) = setup(vec![quota()], &[], &["ghost/model"]);
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::LimitReached { .. }))
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning { message }
                if message.contains("ghost/model has no credentials") && message.contains("fallback")
        )),
        "{events:?}"
    );
}

// The triggers: a rate limit and an unavailable or overloaded provider after the retries, a
// network error after the retries, and a quota error at once.
#[tokio::test]
async fn every_trigger_falls_back() {
    type Make = fn() -> Vec<Script>;
    let cases: [(&str, Make); 5] = [
        ("rate limit", || times(5, || http(429, "{}"))),
        ("503 after retries", || times(5, || http(503, "down"))),
        ("overloaded", || {
            times(5, || ProviderError::Reported {
                status: 529,
                body: "overloaded".into(),
                retry_after: None,
            })
        }),
        ("network", || {
            times(5, || ProviderError::Network("reset".into()))
        }),
        ("quota at once", || vec![quota()]),
    ];
    for (name, failing) in cases {
        let fallback = candidate("fb/x", vec![Script::text("ok")]);
        let (mut agent, primary) = setup(failing(), &[&fallback], &["fb/x"]);
        let (reason, events) = run(&mut agent, "go").await;
        assert_eq!(reason, TurnEndReason::Completed, "{name}: {events:?}");
        assert_eq!(switches(&events).len(), 1, "{name}");
        // Retries came first, except for the quota error.
        let expected = if name == "quota at once" { 1 } else { 5 };
        assert_eq!(primary.requests().len(), expected, "{name}");
    }
}

// Spec "Some failures never fall back": auth, other 4xx, context overflow and the spend cap.
#[tokio::test]
async fn failures_a_chain_cannot_help_with_never_fall_back() {
    type Make = fn() -> Vec<Script>;
    let cases: [(&str, Make); 5] = [
        ("auth", || vec![Script::error(http(401, "bad key"))]),
        ("other 4xx", || {
            vec![Script::error(http(400, "bad request"))]
        }),
        ("forbidden", || vec![Script::error(http(403, "no"))]),
        ("spend cap", || {
            vec![Script::error(http(
                429,
                r#"{"error":{"code":"enforced_spend_limit_reached"}}"#,
            ))]
        }),
        ("context overflow", || {
            vec![Script::error(http(
                400,
                r#"{"error":{"code":"context_length_exceeded"}}"#,
            ))]
        }),
    ];
    for (name, failing) in cases {
        let fallback = candidate("fb/x", vec![Script::text("never")]);
        let (mut agent, _) = setup(failing(), &[&fallback], &["fb/x"]);
        let (reason, events) = run(&mut agent, "go").await;
        assert_eq!(reason, TurnEndReason::Error, "{name}");
        assert!(switches(&events).is_empty(), "{name}: {events:?}");
        assert!(fallback.provider.requests().is_empty(), "{name}");
    }
}

/// Stops requests on an API key, and records every request.
#[derive(Default)]
struct Capped {
    stop: bool,
    records: Mutex<Vec<RequestRecord>>,
}

impl Meter for Capped {
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
        if self.stop && account == AccountKind::ApiKey {
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

// Spec "Budget stop": a budget stops the turn, and no fallback is tried.
#[tokio::test]
async fn a_budget_stop_is_not_a_failure_to_fall_back_from() {
    let fallback = candidate("fb/x", vec![Script::text("never")]);
    let (agent, primary) = setup(vec![Script::text("never")], &[&fallback], &["fb/x"]);
    let mut agent = agent.with_meter(Arc::new(Capped {
        stop: true,
        ..Capped::default()
    }));
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Budget);
    assert!(switches(&events).is_empty());
    assert!(primary.requests().is_empty() && fallback.provider.requests().is_empty());
}

// Budgets apply to a fallback model's requests: a billed candidate is skipped once the budget is
// reached (the failed model here is a ChatGPT plan, which a reached budget does not stop), and
// the next one, which adds no billed cost, is used.
#[tokio::test]
async fn a_billed_candidate_is_skipped_once_a_budget_is_reached() {
    let billed = candidate("openai/gpt-5", vec![Script::text("never")]);
    let subscription = candidate("chatgpt/gpt-5", vec![Script::text("from the plan")]);
    let (agent, _) = setup(
        times(5, || http(503, "down")),
        &[&billed, &subscription],
        &["openai/gpt-5", "chatgpt/gpt-5"],
    );
    let meter = Arc::new(Capped {
        stop: true,
        ..Capped::default()
    });
    let mut agent = agent.with_meter(meter.clone());
    // On a ChatGPT plan, which a reached budget does not stop.
    agent.config_mut().model_id = "chatgpt/m1".into();
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed, "{events:?}");
    // (The first switch is the test changing the model id after the agent was made.)
    assert_eq!(switches(&events).last().unwrap().1, "chatgpt/gpt-5");
    assert!(billed.provider.requests().is_empty());
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning { message }
                if message.contains("openai/gpt-5") && message.contains("daily budget")
        )),
        "{events:?}"
    );
    // The fallback request is metered under the model that answered it.
    let records = meter.records.lock().unwrap();
    let last = records.last().unwrap();
    assert_eq!(
        (last.model.as_str(), last.role.as_str()),
        ("chatgpt/gpt-5", "main")
    );
}

// The switch lasts for the turn: its later steps stay on the fallback model.
#[tokio::test]
async fn the_rest_of_the_turn_stays_on_the_fallback_model() {
    let fallback = candidate(
        "fb/x",
        vec![
            Script::tool_call("c1", "echo", serde_json::json!({"text": "hi"})),
            Script::text("done"),
        ],
    );
    let (mut agent, primary) = setup(vec![quota()], &[&fallback], &["fb/x"]);
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(fallback.provider.requests().len(), 2);
    assert_eq!(primary.requests().len(), 1);
    assert_eq!(switches(&events).len(), 1);
}

// Output already shown cannot be sent again: a failure after some is not a fallback.
#[tokio::test]
async fn a_reply_that_had_begun_is_not_re_sent() {
    use harness_core::provider::ProviderEvent;
    let fallback = candidate("fb/x", vec![Script::text("never")]);
    let (mut agent, _) = setup(
        vec![Script::Reply(vec![
            Ok(ProviderEvent::TextDelta("half an ans".into())),
            Err(http(503, "down")),
        ])],
        &[&fallback],
        &["fb/x"],
    );
    let (reason, events) = run(&mut agent, "go").await;
    assert_eq!(reason, TurnEndReason::Error);
    assert!(switches(&events).is_empty());
    assert!(fallback.provider.requests().is_empty());
}
