//! Ruling P4: the test gate runs when the turn changed anything in the workspace, found by
//! comparing with the turn's checkpoint, `bash`'s changes included; without checkpoints, the edit
//! tools' changed paths decide.

use std::sync::Arc;

use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    checkpoint::Checkpoints,
    engine::{EngineConfig, PermissionEngine},
    event::{AgentEvent, ChangeSource, GateKind, GateStatus},
    gate::Gates,
    permission::Mode,
    testing::{MockProvider, Script},
    tool::ToolContext,
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Run {
    events: Vec<AgentEvent>,
    ws: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl Run {
    /// How the turn's end found out whether files changed, as it was recorded.
    fn checked_by(&self) -> Option<(ChangeSource, bool)> {
        self.events.iter().find_map(|e| match e {
            AgentEvent::ChangesChecked { by, changed } => Some((*by, *changed)),
            _ => None,
        })
    }

    fn test_gate_ran(&self) -> bool {
        self.events.iter().any(|e| {
            matches!(
                e,
                AgentEvent::GateResult {
                    gate: GateKind::Test,
                    status: GateStatus::Passed,
                    ..
                }
            )
        })
    }
}

/// A turn whose only tool call is `calls`, with the test gate `true`, and checkpoints when asked.
async fn turn(calls: Vec<Script>, checkpoints: bool) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir(&ws).unwrap();
    std::fs::write(ws.join("app.py"), "print(1)\n").unwrap();
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: ws.clone(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: true,
        writes_need_approval: false,
    }));
    let mut scripts = calls;
    scripts.push(Script::text("done"));
    let mut agent = Agent::new(
        MockProvider::new(scripts),
        harness_tools::builtin(),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", base.join(".spill")),
        ToolContext::new(&ws),
    )
    .with_gates(Gates {
        test: Some("true".into()),
        ..Gates::default()
    });
    if checkpoints {
        let cp = Checkpoints::open(&base.join("cp/project.git"), &ws, "s1").unwrap();
        agent = agent.with_checkpoints(Some(Arc::new(cp)));
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    agent
        .run_turn("go".to_string(), &tx, CancellationToken::new())
        .await;
    drop(tx);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    Run {
        events,
        ws,
        _dir: dir,
    }
}

fn bash(command: &str) -> Script {
    Script::tool_call("b1", "bash", json!({"command": command}))
}

#[tokio::test]
async fn a_turn_that_changes_a_file_only_through_bash_runs_the_test_gate() {
    let run = turn(
        vec![bash("sed -i.bak 's/1/2/' app.py && rm app.py.bak")],
        true,
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(run.ws.join("app.py")).unwrap(),
        "print(2)\n"
    );
    assert!(run.test_gate_ran(), "{:?}", run.events);
    assert_eq!(run.checked_by(), Some((ChangeSource::Checkpoint, true)));
}

#[tokio::test]
async fn a_turn_whose_bash_changed_nothing_runs_no_test_gate() {
    let run = turn(vec![bash("ls; cat app.py")], true).await;
    assert!(!run.test_gate_ran(), "{:?}", run.events);
    assert_eq!(run.checked_by(), Some((ChangeSource::Checkpoint, false)));
}

// Without checkpoints, only what the edit tools changed is known: bash's change is not seen.
#[tokio::test]
async fn without_checkpoints_the_edit_tools_decide() {
    let by_bash = turn(
        vec![bash("sed -i.bak 's/1/2/' app.py && rm app.py.bak")],
        false,
    )
    .await;
    assert!(!by_bash.test_gate_ran());
    assert_eq!(by_bash.checked_by(), Some((ChangeSource::EditTools, false)));
    let by_edit = turn(
        vec![
            Script::tool_call("r1", "read", json!({"path": "app.py"})),
            Script::tool_call(
                "e1",
                "edit",
                json!({"path": "app.py", "old_string": "print(1)", "new_string": "print(2)"}),
            ),
        ],
        false,
    )
    .await;
    assert!(by_edit.test_gate_ran());
    assert_eq!(by_edit.checked_by(), Some((ChangeSource::EditTools, true)));
}
