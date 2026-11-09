//! M3a: ledger and outcome records name the child agent and its parent, and a child's spend
//! counts towards its parent session's budget.

use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use harness_core::{
    message::Usage,
    meter::{AccountKind, AgentScope, Meter, RequestRecord},
};
use harness_usage::{
    budget::Budgets,
    ledger::{Ledger, LedgerRecord},
    meter::UsageMeter,
    paths::Dirs,
    pricing::{Price, Pricing},
};

fn meter(data: &std::path::Path, budgets: Budgets) -> UsageMeter {
    let now = Arc::new(AtomicU64::new(1_790_942_400));
    let pricing = Pricing::load(
        &data.join("none.json"),
        vec![(
            "openai/gpt-5".into(),
            Price {
                input: Some(1_000_000.0),
                output: Some(1_000_000.0),
                ..Price::default()
            },
        )],
    );
    UsageMeter::open(data, data)
        .with_clock(Arc::new(move || {
            now.load(std::sync::atomic::Ordering::SeqCst)
        }))
        .with_pricing(pricing)
        .with_budgets(budgets)
}

fn request(session: &str, tokens: u64) -> RequestRecord {
    RequestRecord {
        session: session.into(),
        role: "main".into(),
        model: "openai/gpt-5".into(),
        local: false,
        usage: Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
        duration: Duration::from_millis(1),
        outcome: "ok".into(),
    }
}

fn scope() -> AgentScope {
    AgentScope {
        agent: "a1".into(),
        parent: "parent".into(),
    }
}

fn records(data: &std::path::Path) -> Vec<LedgerRecord> {
    Ledger::new(&Dirs::under(data).usage).read()
}

#[test]
fn a_childs_record_names_its_agent_and_parent() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default());
    m.record_request_as(&request("child", 1), Some(&scope()));
    m.record_request(&request("parent", 1));
    let records = records(data.path());
    assert_eq!(records[0].agent.as_deref(), Some("a1"));
    assert_eq!(records[0].parent.as_deref(), Some("parent"));
    assert_eq!(records[1].agent, None);
    assert_eq!(records[1].parent, None);
}

#[test]
fn the_main_agents_ledger_line_has_no_agent_fields() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default());
    m.record_request(&request("main-session", 1));
    let text =
        std::fs::read_to_string(Ledger::new(&Dirs::under(data.path()).usage).files()[0].clone())
            .unwrap();
    assert!(
        !text.contains("\"agent\"") && !text.contains("\"parent\""),
        "{text}"
    );
}

#[test]
fn a_line_from_before_agents_still_reads() {
    let line = r#"{"v":1,"t":1790942400,"session":"s","project":"p","role":"main","model":"m/x","account":"local","input":1,"cache_read":0,"cache_write":0,"cache_write_1h":0,"output":0,"reasoning":0,"billed_usd":0.0,"list_usd":0.0,"price":null,"ms":1,"outcome":"ok","window":null}"#;
    let record: LedgerRecord = serde_json::from_str(line).unwrap();
    assert_eq!(record.agent, None);
    assert_eq!(record.parent, None);
}

#[test]
fn a_childs_spend_counts_towards_the_parent_sessions_budget_once() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(
        data.path(),
        Budgets {
            session_usd: Some(10.0),
            ..Budgets::default()
        },
    );
    m.record_request(&request("parent", 3));
    m.record_request_as(&request("child", 4), Some(&scope()));
    let lines = m.budget_report("parent");
    let session = lines
        .iter()
        .find(|l| l.budget == harness_core::meter::BudgetKind::Session)
        .unwrap();
    assert!((session.spent_usd - 7.0).abs() < 1e-9, "{session:?}");
    m.record_request_as(&request("child", 3), Some(&scope()));
    let status = m.check_budget("parent", AccountKind::ApiKey);
    assert!(status.stop.is_some(), "$10 of $10 stops: {status:?}");
}

#[test]
fn a_sibling_sessions_spend_does_not_count() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(
        data.path(),
        Budgets {
            session_usd: Some(10.0),
            ..Budgets::default()
        },
    );
    m.record_request_as(
        &request("other-child", 9),
        Some(&AgentScope {
            agent: "a1".into(),
            parent: "other-parent".into(),
        }),
    );
    assert!(m.check_budget("parent", AccountKind::ApiKey).stop.is_none());
}

#[test]
fn a_cache_from_before_agents_is_rebuilt_from_the_ledger() {
    use rusqlite::Connection;
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default());
    m.record_request_as(&request("child", 1), Some(&scope()));
    let dirs = Dirs::under(data.path());
    // A cache of schema 1: the table has no parent column.
    {
        let mut store = harness_usage::store::Store::open(&dirs).unwrap();
        store.sync().unwrap();
    }
    let db = Connection::open(dirs.usage.join("index.sqlite")).unwrap();
    db.execute_batch("PRAGMA user_version = 1; DROP TABLE requests; DROP TABLE files; CREATE TABLE requests (file TEXT); CREATE TABLE files (name TEXT PRIMARY KEY, offset INTEGER NOT NULL);").unwrap();
    drop(db);
    let mut store = harness_usage::store::Store::open(&dirs).unwrap();
    store.sync().unwrap();
    assert_eq!(store.session_spent("parent").unwrap(), 1.0);
}

#[test]
fn a_childs_outcome_names_its_agent_and_parent_and_the_mains_does_not() {
    use harness_core::meter::{GateCounts, TurnRecord};
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default());
    let turn = |session: &str| TurnRecord {
        session: session.into(),
        turn: "t".into(),
        role: "main".into(),
        model: "m/x".into(),
        selected_by: "config".into(),
        input_tokens: 1,
        output_tokens: 1,
        first_token_ms: None,
        duration_ms: 1,
        tool_calls: 0,
        invalid_calls: 0,
        retries: 0,
        finish_reason: "completed".into(),
        started_at: 1_790_942_390,
        ended_at: 1_790_942_400,
        gates: GateCounts::default(),
        handoff: None,
    };
    m.record_turn_as(&turn("child"), Some(&scope()));
    m.record_turn(&turn("main-session"));
    let log = harness_usage::outcomes::OutcomeLog::new(&Dirs::under(data.path()).outcomes);
    let records = log.read();
    assert_eq!(records[0].agent.as_deref(), Some("a1"));
    assert_eq!(records[0].parent.as_deref(), Some("parent"));
    assert_eq!(records[1].agent, None);
    let text = serde_json::to_string(&records[1]).unwrap();
    assert!(!text.contains("\"agent\""));
}

#[test]
fn usage_can_be_grouped_by_agent_with_the_main_agent_as_main() {
    use harness_usage::store::{Group, Query, Store};
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default());
    m.record_request(&request("parent", 2));
    m.record_request_as(&request("child", 3), Some(&scope()));
    m.record_request_as(&request("child", 4), Some(&scope()));
    assert_eq!(Group::parse("agent"), Some(Group::Agent));
    let mut store = Store::open(&Dirs::under(data.path())).unwrap();
    store.sync().unwrap();
    let report = store.report(&Query::all(Group::Agent)).unwrap();
    let keys: Vec<(&str, u64)> = report
        .rows
        .iter()
        .map(|r| (r.key.as_str(), r.requests))
        .collect();
    assert_eq!(keys, vec![("a1", 2), ("main", 1)]);
    assert_eq!(report.total.requests, 3);
}

fn window(percent: f64) -> harness_core::meter::WindowSnapshot {
    use harness_core::meter::{Window, WindowSnapshot, WindowSource};
    WindowSnapshot {
        windows: vec![Window {
            window_minutes: Some(300),
            used_percent: Some(percent),
            resets_at: Some(1_790_946_000),
            source: WindowSource::Header,
        }],
        observed_at: 1_790_942_400,
    }
}

#[test]
fn a_window_snapshot_is_named_by_the_agent_that_saw_it_and_not_by_whoever_records_first() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(data.path(), Budgets::default());
    let (childs, mains) = (window(30.0), window(60.0));
    // Both requests are in flight; each saw its own snapshot; the main agent's ends first.
    m.record_window_as(&childs, Some(&scope()));
    m.record_window(&mains);
    m.record_request(&request("main-session", 1));
    m.record_request_as(&request("child", 1), Some(&scope()));
    let records = records(data.path());
    assert_eq!(records[0].window.as_deref(), Some(mains.id().as_str()));
    assert_eq!(records[1].window.as_deref(), Some(childs.id().as_str()));
}

#[test]
fn a_child_leaves_the_eighty_percent_warning_to_the_main_agent() {
    let data = tempfile::tempdir().unwrap();
    let m = meter(
        data.path(),
        Budgets {
            session_usd: Some(10.0),
            ..Budgets::default()
        },
    );
    m.record_request_as(&request("child", 8), Some(&scope()));
    // The child checks first, and is told nothing of it.
    let childs = m.check_budget_as("parent", AccountKind::ApiKey, Some(&scope()));
    assert!(
        childs.warnings.is_empty() && childs.stop.is_none(),
        "{childs:?}"
    );
    // The main agent's next check says it, once.
    let first = m.check_budget("parent", AccountKind::ApiKey);
    assert_eq!(first.warnings.len(), 1, "{first:?}");
    assert!(
        m.check_budget("parent", AccountKind::ApiKey)
            .warnings
            .is_empty()
    );
}
