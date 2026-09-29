//! A mode change between turns: the note that records it, and how it reaches the provider.

mod common;

use std::{path::Path, sync::Arc};

use common::*;
use harness_core::{
    agent::{Agent, AgentConfig, NonInteractive},
    engine::{EngineConfig, PermissionEngine},
    message::Message,
    permission::{FsAccess, Mode},
    testing::{MockProvider, Script},
    tool::{CommandSandbox, ToolContext, ToolRegistry},
};

/// A sandbox that is never used: it only makes the agent's tool context have one.
#[derive(Debug)]
struct Present;
impl CommandSandbox for Present {
    fn name(&self) -> &'static str {
        "present"
    }
    fn command(
        &self,
        _access: FsAccess,
        _workspace: &Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        Ok(cmd)
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
}

/// An agent in `auto` mode, with an OS sandbox for shell commands when `sandboxed`.
fn agent_in(dir: &Path, sandboxed: bool) -> Agent {
    let sandbox: Option<Arc<dyn CommandSandbox>> = sandboxed.then(|| Arc::new(Present) as _);
    let policy = Arc::new(PermissionEngine::new(EngineConfig {
        mode: Mode::Auto,
        workspace: dir.to_path_buf(),
        read_dirs: vec![],
        rules: Default::default(),
        sandbox_available: sandboxed,
        writes_need_approval: false,
    }));
    Agent::new(
        MockProvider::new(vec![]),
        ToolRegistry::new(vec![]),
        policy,
        Arc::new(NonInteractive),
        AgentConfig::new("mock/m1", "m1", "system prompt", dir.join(".spill")),
        ToolContext::new(dir).with_sandbox(sandbox, FsAccess::WorkspaceWrite),
    )
}

/// The note `set_mode(mode)` appends.
fn note(agent: &mut Agent, mode: Mode) -> String {
    agent.set_mode(mode);
    match agent.history().last() {
        Some(Message::User { content }) => content.clone(),
        other => panic!("{other:?}"),
    }
}

// Review C, minor 3: the note says what the new mode allows with the sandbox the session has, as
// the system prompt does.
#[test]
fn the_mode_change_note_says_what_the_mode_allows_with_or_without_a_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let mut with = agent_in(dir.path(), true);
    let mut without = agent_in(dir.path(), false);

    let plan = note(&mut with, Mode::Plan);
    assert!(plan.contains("read-only sandbox"), "{plan}");
    let plan = note(&mut without, Mode::Plan);
    assert!(
        plan.contains("shell commands are refused") && plan.contains("no OS sandbox"),
        "{plan}"
    );

    let auto = note(&mut with, Mode::Auto);
    assert!(auto.contains("run without approval"), "{auto}");
    let auto = note(&mut without, Mode::Auto);
    assert!(
        auto.contains("every shell command needs approval")
            && auto.contains("no OS sandbox")
            && !auto.contains("run without approval"),
        "{auto}"
    );

    let ask = note(&mut without, Mode::Ask);
    assert!(
        ask.contains("every shell command needs approval") && ask.contains("no OS sandbox"),
        "{ask}"
    );

    for agent in [&mut with, &mut without] {
        let full = note(agent, Mode::FullAccess);
        assert!(
            full.starts_with("[harness] The approval mode is now full-access")
                && full.contains("forbids or may match"),
            "{full}"
        );
    }
    assert!(note(&mut with, Mode::FullAccess).contains("still run in the sandbox"));
    assert!(note(&mut without, Mode::FullAccess).contains("without approval or sandbox"));
}

// Review C, minor 4: some chat templates require user and assistant messages to alternate, so the
// note and the next prompt reach the provider as one user message.
#[tokio::test]
async fn a_note_and_the_next_prompt_are_sent_as_one_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let provider = MockProvider::new(vec![Script::text("one"), Script::text("two")]);
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    );
    run(&mut agent, "first").await;
    agent.set_mode(Mode::Plan);
    run(&mut agent, "second").await;

    let messages = &provider.requests()[1].messages;
    for pair in messages.windows(2) {
        assert!(
            !matches!(pair, [Message::User { .. }, Message::User { .. }]),
            "{messages:?}"
        );
    }
    match messages.last() {
        Some(Message::User { content }) => {
            assert!(
                content.starts_with("[harness] The approval mode is now plan")
                    && content.ends_with("\n\nsecond"),
                "{content}"
            );
        }
        other => panic!("{other:?}"),
    }
    // The session keeps them apart: the note is still its own entry.
    let history = agent.history();
    assert!(matches!(
        &history[history.len() - 3..],
        [
            Message::User { .. },
            Message::User { .. },
            Message::Assistant { .. }
        ]
    ));
}
