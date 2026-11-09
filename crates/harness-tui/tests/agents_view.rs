//! M3b: a subagent that ends leaves one line in the transcript, and `/agents` lists the agents
//! the model can start (and where each came from) and the children of this session.

mod common;

use std::path::PathBuf;

use common::*;
use harness_core::{
    agents::{
        AgentDefinition, AgentId, AgentInfo, AgentStatus, AgentTag, DefinitionSource, Definitions,
    },
    permission::Mode,
    testing::MockProvider,
};
use harness_tui::app::{Host, Prepared};

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

fn open() -> (
    tempfile::TempDir,
    harness_tui::ui::Ui<ratatui::backend::TestBackend>,
) {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![]), dir.path(), Mode::Auto);
    let mut options = options(dir.path(), Mode::Auto);
    options.commands = vec![("agents".into(), "List the subagents".into())];
    let (ui, _) = start(agent, Box::new(NoCommands), options);
    (dir, ui)
}

fn child(id: &str, label: &str, status: AgentStatus) -> AgentInfo {
    AgentInfo {
        tag: AgentTag {
            id: AgentId(id.into()),
            label: label.into(),
        },
        status,
        model: Some("mock/m1".into()),
        tool_calls: 3,
        activity: None,
        summary: None,
        error: None,
        started: None,
    }
}

// ---- a finished child's line ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_child_that_finished_leaves_one_line_with_what_it_found() {
    let (_dir, mut ui) = open();
    let mut done = child("a1", "explore", AgentStatus::Done);
    done.summary = Some("The entry point is src/main.rs.\nIt calls run().".into());
    ui.app_mut().set_children(vec![done.clone()]);
    ui.draw().unwrap();
    let lines: Vec<String> = everything(&ui)
        .into_iter()
        .filter(|r| r.contains("a1"))
        .collect();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let line = &lines[0];
    assert!(line.contains("explore"), "{line}");
    assert!(line.contains("done"), "{line}");
    assert!(line.contains("3 tool calls"), "{line}");
    // The model it ran on, so a spawn that named another one is seen when it ends.
    assert!(line.contains("mock/m1"), "{line}");
    assert!(line.contains("The entry point is src/main.rs."), "{line}");
    // Only the first line of its report, on one row.
    assert!(!line.contains("It calls run()"), "{line}");
    // The same child seen again (every change redraws) is not announced again.
    ui.app_mut().set_children(vec![done]);
    ui.draw().unwrap();
    assert_eq!(
        everything(&ui).iter().filter(|r| r.contains("a1")).count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_child_has_no_result_line_until_it_ends() {
    let (_dir, mut ui) = open();
    ui.app_mut()
        .set_children(vec![child("a1", "explore", AgentStatus::Working)]);
    ui.draw().unwrap();
    assert!(!everything(&ui).iter().any(|r| r.contains("finished")));
    ui.app_mut()
        .set_children(vec![child("a1", "explore", AgentStatus::Done)]);
    ui.draw().unwrap();
    assert!(
        everything(&ui)
            .iter()
            .any(|r| r.contains("explore") && r.contains("done"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_child_says_why_a_cancelled_one_says_so_and_a_done_without_tools_is_flagged() {
    let (_dir, mut ui) = open();
    let mut failed = child("a1", "review", AgentStatus::Failed);
    failed.error = Some("it stopped at its step limit".into());
    let cancelled = child("a2", "explore", AgentStatus::Cancelled);
    let mut idle = child("a3", "explore", AgentStatus::Done);
    idle.tool_calls = 0;
    idle.summary = Some("all fine".into());
    ui.app_mut().set_children(vec![failed, cancelled, idle]);
    ui.draw().unwrap();
    let all = everything(&ui);
    let row = |id: &str| {
        all.iter()
            .find(|r| r.contains(id))
            .cloned()
            .unwrap_or_default()
    };
    assert!(
        row("a1").contains("failed") && row("a1").contains("step limit"),
        "{all:#?}"
    );
    assert!(row("a2").contains("cancelled"), "{all:#?}");
    assert!(row("a3").contains("no tool calls"), "{all:#?}");
    assert!(row("a3").contains("unchecked"), "{all:#?}");
    // Only a done child with no calls is flagged.
    assert!(!row("a1").contains("unchecked"), "{all:#?}");
    assert!(!row("a2").contains("unchecked"), "{all:#?}");
}

// ---- /agents -------------------------------------------------------------------------------

fn definitions() -> Definitions {
    let triage = AgentDefinition {
        tools: Some(vec!["read".into(), "grep".into()]),
        mode: Some(Mode::ReadOnly),
        model: None,
        model_ignored: Some("ollama/qwen3:14b".into()),
        source: DefinitionSource::Project(PathBuf::from("/repo/.harness/agents/triage.md")),
        ..AgentDefinition::new("triage", "Sorts failing tests", "Look.")
    };
    let mine = AgentDefinition {
        model: Some("ollama/x".into()),
        source: DefinitionSource::User(PathBuf::from("/home/me/.config/harness/agents/mine.md")),
        ..AgentDefinition::new("mine", "My helper", "Help.")
    };
    Definitions::new(vec![triage, mine])
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_lists_each_definition_with_where_it_came_from_and_what_it_gets() {
    let (_dir, mut ui) = open();
    ui.app_mut().set_agent_definitions(definitions());
    send(&mut ui, "/agents");
    let all = everything(&ui);
    let at = all
        .iter()
        .position(|r| r.contains("Agents"))
        .unwrap_or_else(|| panic!("{all:#?}"));
    let text = all[at..].join("\n");
    for name in ["explore", "review", "general", "triage", "mine"] {
        assert!(text.contains(name), "{text}");
    }
    assert!(text.contains("built-in"), "{text}");
    assert!(text.contains("/repo/.harness/agents/triage.md"), "{text}");
    assert!(text.contains("Sorts failing tests"), "{text}");
    assert!(text.contains("tools read, grep"), "{text}");
    assert!(text.contains("mode read-only"), "{text}");
    assert!(text.contains("model ollama/x"), "{text}");
    // A project file's model that is not used because the workspace is not trusted.
    assert!(text.contains("ollama/qwen3:14b"), "{text}");
    assert!(text.contains("not trusted"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_lists_the_children_of_this_session_finished_ones_too() {
    let (_dir, mut ui) = open();
    ui.app_mut().set_agent_definitions(definitions());
    let mut failed = child("a2", "review", AgentStatus::Failed);
    failed.error = Some("it stopped at its step limit".into());
    ui.app_mut().set_children(vec![
        child("a1", "explore", AgentStatus::Done),
        failed,
        child("a3", "general", AgentStatus::Working),
    ]);
    send(&mut ui, "/agents");
    let all = everything(&ui);
    let at = all
        .iter()
        .rposition(|r| r.contains("Subagents"))
        .unwrap_or_else(|| panic!("{all:#?}"));
    let text = all[at..].join("\n");
    assert!(text.contains("a1") && text.contains("done"), "{text}");
    assert!(
        text.contains("a2") && text.contains("failed") && text.contains("step limit"),
        "{text}"
    );
    assert!(text.contains("a3") && text.contains("working"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_children_agents_says_so() {
    let (_dir, mut ui) = open();
    ui.app_mut().set_agent_definitions(definitions());
    send(&mut ui, "/agents");
    assert!(
        shows(&ui, "No subagents have run in this session"),
        "{:#?}",
        everything(&ui)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_shows_what_a_child_says_with_secrets_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let agent = agent(MockProvider::new(vec![]), dir.path(), Mode::Auto);
    let mut options = options(dir.path(), Mode::Auto);
    options.commands = vec![("agents".into(), "List the subagents".into())];
    let (ui, _) = start(agent, Box::new(NoCommands), options);
    let redactor = std::sync::Arc::new(harness_core::redact::Redactor::default());
    redactor.add("sk-leaky-secret-123456");
    let mut ui = ui.with_redactor(redactor);
    let mut failed = child("a2", "review", AgentStatus::Failed);
    failed.error = Some("the server said sk-leaky-secret-123456 is wrong".into());
    failed.model = Some("sk-leaky-secret-123456/m".into());
    ui.app_mut().set_agent_definitions(definitions());
    ui.app_mut().set_children(vec![failed]);
    send(&mut ui, "/agents");
    let all = everything(&ui);
    assert!(
        all.iter().any(|r| r.contains("a2") && r.contains("failed")),
        "{all:#?}"
    );
    assert!(
        !all.iter().any(|r| r.contains("sk-leaky-secret")),
        "{all:#?}"
    );
}
