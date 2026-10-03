//! The four roles, their names, and what `/roles` lists: each role's model and where it came
//! from.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::{
    agent::{Agent, NonInteractive, SessionModel},
    message::RequestOptions,
    permission::Mode,
    role::{Role, RoleConfig, RoleLine, RoleSource},
    testing::MockProvider,
};

#[test]
fn the_roles_are_named_and_an_unknown_one_lists_them() {
    assert_eq!(
        Role::ALL.map(Role::as_str),
        ["main", "plan", "build", "background"]
    );
    for role in Role::ALL {
        assert_eq!(role.as_str().parse::<Role>(), Ok(role));
    }
    // Spec "Unknown role": an error lists `main`, `plan`, `build` and `background`.
    let error = "review".parse::<Role>().unwrap_err();
    assert_eq!(
        error,
        "unknown role `review`; the roles are main, plan, build and background"
    );
}

/// The session continues on `id`, as `/model` does.
fn switch(agent: &mut Agent, id: &str) {
    agent.switch_model(SessionModel {
        provider: MockProvider::new(vec![]),
        id: id.into(),
        name: id.rsplit('/').next().unwrap().into(),
        context_window: 100_000,
        request: RequestOptions::default(),
        text_tool_calls: false,
        tools: None,
        edit_section: None,
    });
}

fn lines(config: RoleConfig, chosen: bool) -> Vec<RoleLine> {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = agent(
        MockProvider::new(vec![]),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_roles(config);
    if chosen {
        switch(&mut agent, "ollama/llama3");
    }
    agent.role_lines()
}

fn line(role: Role, model: &str, source: RoleSource) -> RoleLine {
    RoleLine {
        role,
        model: model.into(),
        source,
    }
}

// Spec "Defaults": nothing configured, so every role is the session's model.
#[test]
fn with_no_roles_every_role_is_main() {
    assert_eq!(
        lines(RoleConfig::default(), false),
        [
            line(Role::Main, "mock/m1", RoleSource::Config),
            line(Role::Plan, "mock/m1", RoleSource::Inherited),
            line(Role::Build, "mock/m1", RoleSource::Inherited),
            line(Role::Background, "mock/m1", RoleSource::Inherited),
        ]
    );
}

// Spec "Showing roles": `plan` from config, `build` from the session, `main` as
// `ollama/llama3`, and `background` inherited from `main`.
#[test]
fn each_role_says_where_its_model_came_from() {
    let config = RoleConfig {
        plan: Some("chatgpt/gpt-5".into()),
        ..RoleConfig::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let mut agent = self::agent(
        MockProvider::new(vec![]),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_roles(config);
    agent.set_role_model(Role::Build, "ollama/qwen3-coder");
    switch(&mut agent, "ollama/llama3");
    assert_eq!(
        agent.role_lines(),
        [
            line(Role::Main, "ollama/llama3", RoleSource::Session),
            line(Role::Plan, "chatgpt/gpt-5", RoleSource::Config),
            line(Role::Build, "ollama/qwen3-coder", RoleSource::Session),
            line(Role::Background, "ollama/llama3", RoleSource::Inherited),
        ]
    );
}

#[test]
fn a_role_set_for_the_session_goes_over_the_configured_one() {
    let config = RoleConfig {
        plan: Some("chatgpt/gpt-5".into()),
        ..RoleConfig::default()
    };
    let lines = lines(config, true);
    assert_eq!(
        lines[1],
        line(Role::Plan, "chatgpt/gpt-5", RoleSource::Config)
    );
    assert_eq!(lines[0].source, RoleSource::Session);
}
