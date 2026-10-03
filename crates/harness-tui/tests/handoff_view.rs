//! Build on another model: the choice starts a turn on the `build` role, announced, and a plan
//! that goes alone is said so.

mod common;

use std::{collections::HashMap, sync::Arc};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    permission::Mode,
    role::{ModelResolver, RoleConfig},
    testing::{MockProvider, Script},
    turn::TurnModel,
};
use harness_tui::app::{Host, Prepared};
use tokio_util::sync::CancellationToken;

struct NoCommands;

impl Host for NoCommands {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
}

struct Models(HashMap<String, TurnModel>);

impl ModelResolver for Models {
    fn resolve(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<TurnModel, String>> {
        let found = self.0.get(id).cloned().ok_or_else(|| format!("no {id}"));
        Box::pin(async move { found })
    }
}

fn model(id: &str, provider: &Arc<MockProvider>, window: u64) -> TurnModel {
    TurnModel {
        provider: provider.clone(),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        local: false,
        tools: None,
        edit_section: None,
        context_window: Some(window),
        request: None,
        text_tool_calls: false,
    }
}

/// Plans on `plan/big`, then builds on `build/small` with `build_window` tokens.
async fn plan_then_build(
    question_chars: usize,
    plan_text: &str,
    build_window: u64,
) -> (
    tempfile::TempDir,
    harness_tui::ui::Ui<ratatui::backend::TestBackend>,
    Arc<MockProvider>,
) {
    let dir = tempfile::tempdir().unwrap();
    let planner = MockProvider::new(vec![Script::text(plan_text)]);
    let builder = MockProvider::new(vec![Script::text("built it")]);
    let models = Models(
        [
            model("plan/big", &planner, 1_000_000),
            model("build/small", &builder, build_window),
        ]
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect(),
    );
    let agent = agent(MockProvider::new(vec![]), dir.path(), Mode::Plan)
        .with_roles(RoleConfig {
            plan: Some("plan/big".into()),
            build: Some("build/small".into()),
            ..RoleConfig::default()
        })
        .with_resolver(Arc::new(models));
    let (mut ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Plan));
    // Pasted: a question this long is not typed key by key.
    ui.handle(ratatui::crossterm::event::Event::Paste(
        "q".repeat(question_chars),
    ))
    .unwrap();
    press(&mut ui, ratatui::crossterm::event::KeyCode::Enter);
    settle(&mut ui).await;
    until_armed(&ui).await;
    press(&mut ui, ratatui::crossterm::event::KeyCode::Char('b'));
    settle(&mut ui).await;
    (dir, ui, builder)
}

// Spec "Build on the build role": the turn runs on the build model, a `ModelSwitched` line is
// shown, and the whole history goes along when it fits.
#[tokio::test]
async fn build_runs_on_the_build_role_and_says_so() {
    let (_dir, ui, builder) = plan_then_build(100, "1. Do it", 200_000).await;
    assert!(
        shows(
            &ui,
            "switched to build/small (build role, from plan/big; you chose it)"
        ),
        "{:#?}",
        everything(&ui)
    );
    assert_eq!(builder.requests().len(), 1);
    assert!(!shows(&ui, "gets the plan alone"));
    assert!(shows(&ui, "built it"));
}

// Spec "History does not fit": a notice is shown.
#[tokio::test]
async fn a_plan_that_goes_alone_is_said_so() {
    let (_dir, ui, builder) = plan_then_build(12_000, "1. Do it", 3_000).await;
    assert!(
        shows_wrapped(&ui, "so the build turn gets the plan alone"),
        "{:#?}",
        everything(&ui)
    );
    assert_eq!(builder.requests()[0].messages.len(), 1);
    // The message that points at the plan carries it, since the plan is all the model gets.
    let sent = &builder.requests()[0].messages[0];
    assert!(
        matches!(sent, harness_core::message::Message::User { content } if content.contains("1. Do it")),
        "{sent:?}"
    );
}
