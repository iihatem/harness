//! `/roles`: each role's effective model and where it came from, as the terminal shows it.

mod common;

use common::*;
use harness_core::{
    permission::Mode,
    role::{Role, RoleConfig},
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

fn open(
    config: RoleConfig,
) -> (
    tempfile::TempDir,
    harness_tui::ui::Ui<ratatui::backend::TestBackend>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(MockProvider::new(vec![]), dir.path(), Mode::Auto).with_roles(config);
    agent.set_role_model(Role::Build, "ollama/qwen3-coder");
    let (ui, _) = start(agent, Box::new(NoCommands), options(dir.path(), Mode::Auto));
    (dir, ui)
}

// Spec "Showing roles", and the `/roles` snapshot of tasks.md 3.1.
#[tokio::test]
async fn roles_lists_each_role_with_its_model_and_where_it_came_from() {
    let (_dir, mut ui) = open(RoleConfig {
        plan: Some("chatgpt/gpt-5".into()),
        ..RoleConfig::default()
    });
    send(&mut ui, "/roles");
    let screen = everything(&ui);
    let at = screen
        .iter()
        .position(|row| row.contains("Roles"))
        .unwrap_or_else(|| panic!("no roles list: {screen:#?}"));
    assert_eq!(
        &screen[at..at + 5],
        [
            "Roles",
            "  main        mock/m              config",
            "  plan        chatgpt/gpt-5       config",
            "  build       ollama/qwen3-coder  session",
            "  background  mock/m              inherited from main",
        ],
        "{screen:#?}"
    );
}
