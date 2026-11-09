//! A parent turn whose in-process children can edit the shared checkout takes its checkpoint
//! before they start, so `/rewind` of the parent restores what they changed. Every model is a
//! mock; the checkpoints are real.

mod common;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use common::{Put, agent_with_sandbox, run};
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    agents::{
        AgentFactory, AgentSpec, ChildSetup, Definitions, Endpoint, Limits, ParentContext,
        ParentModel, SpawnAgentsTool, SpawnSettings, Supervisor,
    },
    checkpoint::Checkpoints,
    engine::{EngineConfig, PermissionEngine, RuleSet},
    permission::Mode,
    session::{RewindScope, Session},
    testing::{MockProvider, Script},
    tool::{ToolContext, ToolRegistry},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn checkpoint_count(events: &[harness_core::event::AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, harness_core::event::AgentEvent::CheckpointCreated { .. }))
        .count()
}

struct Factory {
    dir: PathBuf,
    scripts: Mutex<std::collections::VecDeque<Vec<Script>>>,
}

#[async_trait]
impl AgentFactory for Factory {
    fn endpoint(&self, _spec: &AgentSpec) -> Endpoint {
        Endpoint {
            key: "mock".into(),
            local: false,
            delegated: false,
        }
    }
    async fn build(&self, setup: ChildSetup) -> Result<Agent, String> {
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        let mode = setup.mode;
        Ok(Agent::new(
            MockProvider::new(script),
            ToolRegistry::new(vec![Arc::new(Put)]),
            setup.policy,
            setup.approver,
            AgentConfig::new("mock/m1", "m1", "prompt", self.dir.join(".spill")),
            ToolContext::new(&self.dir).with_sandbox(None, mode.fs_access()),
        )
        .with_session(setup.session))
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    ws: PathBuf,
    agent: Agent,
}

/// A parent agent (with checkpoints) that has the spawn tool, and a supervisor whose children
/// follow `child_scripts` one after another.
fn rig(
    parent: Vec<Script>,
    child_scripts: Vec<Vec<Script>>,
    mode: Mode,
) -> (Rig, Arc<MockProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let ws = base.join("ws");
    std::fs::create_dir(&ws).unwrap();
    let data = base.join("data");
    let config = EngineConfig {
        mode,
        workspace: ws.clone(),
        read_dirs: vec![],
        rules: RuleSet::default(),
        sandbox_available: true,
        writes_need_approval: false,
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let supervisor = Supervisor::new(
        Limits {
            per_local: 4,
            per_hosted: 4,
            per_delegated: 4,
            total: 4,
        },
        Arc::new(Factory {
            dir: ws.clone(),
            scripts: Mutex::new(child_scripts.into()),
        }),
        ParentContext {
            session: "parent".into(),
            cancel: CancellationToken::new(),
            policy: Arc::new(PermissionEngine::new(config.clone())),
            engine: config,
            approver: Arc::new(NonInteractive),
            meter: None,
            sessions_dir: None,
            workspace: ws.clone(),
        },
        tx,
    );
    let tool = SpawnAgentsTool::new(
        &supervisor,
        Arc::new(Definitions::new(Vec::new())),
        SpawnSettings {
            max_per_turn: 4,
            background_model: None,
            parent: ParentModel::default(),
            also_in_play: vec![],
            wait_for_background: true,
        },
    );
    let session = Session::create(&data.join("sessions"), &ws);
    let checkpoints = Checkpoints::open(&data.join("checkpoints.git"), &ws, session.id()).unwrap();
    let provider = MockProvider::new(parent);
    let agent = agent_with_sandbox(provider.clone(), mode, Arc::new(NonInteractive), &ws)
        .with_session(session)
        .with_checkpoints(Some(Arc::new(checkpoints)))
        .with_extra_tool(tool);
    // The supervisor lives as long as the tool the agent holds weakly: keep it.
    std::mem::forget(supervisor);
    (
        Rig {
            _dir: dir,
            ws,
            agent,
        },
        provider,
    )
}

fn spawn(agent: &str, task: &str) -> Script {
    Script::tool_call(
        "s1",
        "spawn_agents",
        json!({"agents": [{"agent": agent, "task": task}]}),
    )
}

#[tokio::test]
async fn rewinding_a_turn_whose_subagent_edited_the_checkout_restores_its_edits() {
    let (mut rig, _) = rig(
        vec![spawn("general", "write child.txt"), Script::text("done")],
        vec![vec![
            Script::tool_call(
                "p1",
                "put",
                json!({"path": "child.txt", "content": "from the child\n"}),
            ),
            Script::text("wrote it"),
        ]],
        Mode::Auto,
    );
    std::fs::write(rig.ws.join("keep.txt"), "kept\n").unwrap();
    let (_, events) = run(&mut rig.agent, "have a helper write a file").await;
    assert_eq!(checkpoint_count(&events), 1, "{events:?}");
    assert!(rig.ws.join("child.txt").exists(), "the child wrote it");

    let target = rig
        .agent
        .rewind_points()
        .into_iter()
        .find(|p| p.text.contains("helper"))
        .expect("a rewind point for the turn")
        .entry;
    rig.agent
        .rewind(&target, RewindScope::CodeAndConversation)
        .await
        .unwrap();
    assert!(
        !rig.ws.join("child.txt").exists(),
        "the child's file is gone"
    );
    assert_eq!(
        std::fs::read_to_string(rig.ws.join("keep.txt")).unwrap(),
        "kept\n"
    );
}

#[tokio::test]
async fn a_turn_whose_subagents_cannot_edit_takes_no_checkpoint() {
    let (mut rig, _) = rig(
        vec![spawn("explore", "look around"), Script::text("done")],
        vec![vec![Script::text("nothing to see")]],
        Mode::Auto,
    );
    let (_, events) = run(&mut rig.agent, "look around").await;
    assert_eq!(checkpoint_count(&events), 0, "{events:?}");
}
