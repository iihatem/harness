//! The test gate's comparison with the turn's checkpoint, and the meter, count nothing twice: the
//! comparison makes no snapshot and no request, the gate's command is no tool call of the model's,
//! and each reply is one metered request, the continuation after a failed test included.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    checkpoint::Checkpoints,
    engine::{EngineConfig, PermissionEngine},
    event::AgentEvent,
    gate::Gates,
    meter::{AccountKind, Avoided, Meter, RequestCost, RequestRecord, TurnRecord},
    permission::Mode,
    session::Session,
    testing::{MockProvider, Script},
    tool::ToolContext,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Counting {
    requests: AtomicUsize,
    turns: Mutex<Vec<TurnRecord>>,
}

impl Meter for Counting {
    fn record_request(&self, _request: &RequestRecord) -> RequestCost {
        self.requests.fetch_add(1, Ordering::SeqCst);
        RequestCost {
            account: AccountKind::ApiKey,
            billed_usd: Some(0.0),
            list_usd: Some(0.0),
            avoided: Avoided::NotApplicable,
        }
    }
    fn record_turn(&self, turn: &TurnRecord) {
        self.turns.lock().unwrap().push(turn.clone());
    }
}

// The model changes a file with `bash`, the checkpoint comparison finds it, the test fails once
// and then passes: one turn record, one checkpoint, and as many requests as replies.
#[tokio::test]
async fn the_comparison_and_the_gate_add_no_checkpoint_request_or_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir(&ws).unwrap();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: ws.clone(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    // The test passes once `ok.txt` exists: the model's second command makes it.
    let provider = MockProvider::new(vec![
        Script::tool_call("b1", "bash", json!({"command": "echo x > a.txt"})),
        Script::text("done"),
        Script::tool_call("b2", "bash", json!({"command": "echo x > ok.txt"})),
        Script::text("fixed"),
    ]);
    let session = Session::create(&base.join("sessions"), &ws);
    let checkpoints = Checkpoints::open(&base.join("cp/project.git"), &ws, session.id()).unwrap();
    let meter = Arc::new(Counting::default());
    let mut agent = Agent::new(
        provider.clone(),
        harness_tools::builtin(),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", base.join(".spill")),
        ToolContext::new(&ws),
    )
    .with_session(session)
    .with_checkpoints(Some(Arc::new(checkpoints)))
    .with_meter(meter.clone())
    .with_gates(Gates {
        test: Some("test -f ok.txt".into()),
        max_retries: 3,
        ..Gates::default()
    });
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent
        .run_turn("go".to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    let turns = meter.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 1, "{events:?}");
    let turn = &turns[0];
    assert_eq!(turn.finish_reason, "completed", "{events:?}");
    // The model's two `bash` calls; the test's two runs and the comparisons are not calls.
    assert_eq!(turn.tool_calls, 2);
    assert_eq!(turn.invalid_calls, 0);
    assert_eq!((turn.gates.passed, turn.gates.failed), (1, 1));
    // The failed test's continuation is a request like the others, metered once.
    assert_eq!(provider.requests().len(), 4);
    assert_eq!(meter.requests.load(Ordering::SeqCst), 4);
    // One snapshot, before the turn's first change; comparing makes none. The turn record's
    // retries are the provider's, which a gate's continuation is not.
    let snapshots = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::CheckpointCreated { .. }))
        .count();
    assert_eq!(snapshots, 1);
    assert_eq!(turn.retries, 0);
}
