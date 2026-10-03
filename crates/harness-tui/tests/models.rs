//! `/model`: the model picker, fed by the models the host finds, and switching the session's
//! model between turns, with the status line and `/context` following the new model's window.

mod common;

use std::{collections::HashMap, sync::Arc};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    agent::SessionModel,
    message::{Message, RequestOptions},
    permission::Mode,
    testing::{MockProvider, Script},
};
use harness_tui::{
    app::{Host, ModelSwitch, Prepared},
    ui::Ui,
};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use tokio_util::sync::CancellationToken;

/// Models by id, each on a mock provider, with its window.
#[derive(Default)]
struct Models {
    models: HashMap<String, (Arc<MockProvider>, u64)>,
    /// Switching waits until it is cancelled.
    stuck: bool,
    /// The models the session reported as having answered.
    answered: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Host for Models {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn model_answered(&self, model: &str) -> Vec<String> {
        self.answered.lock().unwrap().push(model.to_string());
        vec![format!("saved {model} as your default model")]
    }
    fn models(&self) -> BoxFuture<'static, Vec<String>> {
        let mut ids: Vec<String> = self.models.keys().cloned().collect();
        ids.sort();
        Box::pin(async move { ids })
    }
    fn switch_model(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ModelSwitch, String>> {
        let found = self.models.get(id).cloned();
        let stuck = self.stuck;
        let id = id.to_string();
        Box::pin(async move {
            if stuck {
                cancel.cancelled().await;
                return Err("stopped".into());
            }
            let (provider, window) = found.ok_or_else(|| {
                format!(
                    "unknown provider `{}`",
                    id.split('/').next().unwrap_or_default()
                )
            })?;
            Ok(ModelSwitch {
                model: SessionModel {
                    provider,
                    name: id.split_once('/').unwrap().1.to_string(),
                    id,
                    context_window: window,
                    request: RequestOptions::default(),
                    text_tool_calls: false,
                },
                window_note: "from the model's profile".into(),
                warnings: vec!["a warning about the window".into()],
            })
        })
    }
}

fn open(first: Arc<MockProvider>, host: Models) -> (Ui<TestBackend>, Log, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (ui, log) = start(
        agent(first, dir.path(), Mode::Auto),
        Box::new(host),
        options(dir.path(), Mode::Auto),
    );
    (ui, log, dir)
}

fn status(ui: &Ui<TestBackend>) -> String {
    screen(ui)
        .into_iter()
        .rfind(|r| r.contains(" · auto · "))
        .unwrap_or_default()
}

fn two_models() -> (Arc<MockProvider>, Arc<MockProvider>, Models) {
    let first = MockProvider::new(vec![Script::text("from the first")]);
    let big = MockProvider::new(vec![Script::text("from the big one")]);
    let mut host = Models::default();
    host.models.insert("mock/m".into(), (first.clone(), 32_768));
    host.models
        .insert("mock/big".into(), (big.clone(), 200_000));
    (first, big, host)
}

#[tokio::test]
async fn model_alone_lists_the_models_and_switches_to_the_chosen_one() {
    let (first, big, host) = two_models();
    let (mut ui, log, _dir) = open(first.clone(), host);
    send(&mut ui, "hello");
    settle(&mut ui).await;
    send(&mut ui, "/model");
    assert_eq!(screen(&ui)[0], "Choose a model");
    until(&mut ui, |app| {
        app.picker().is_some_and(|p| !p.items().is_empty())
    })
    .await;
    let shown = screen(&ui);
    assert!(
        shown
            .iter()
            .any(|r| r.contains("mock/m ") && r.contains("(current)")),
        "{shown:#?}"
    );
    // Typed as the list appears, a key chooses nothing: the list takes keys after a pause.
    let before = ui.app().picker().unwrap().selected();
    type_text(&mut ui, "big");
    press(&mut ui, KeyCode::Down);
    assert!(ui.app().picker().is_some());
    assert_eq!(ui.app().picker().unwrap().selected(), before);
    until_armed(&ui).await;
    type_text(&mut ui, "big");
    press(&mut ui, KeyCode::Enter);
    settle(&mut ui).await;
    assert_eq!(*log.lock().unwrap(), ["enter", "leave"]);
    assert!(shows(&ui, "switched to mock/big"));
    assert!(shows(&ui, "warning: a warning about the window"));
    // What was typed ahead went to the input.
    assert_eq!(ui.app().editor().text(), "big");
    ctrl(&mut ui, 'u');
    assert!(
        status(&ui).starts_with("mock/big · auto"),
        "{}",
        status(&ui)
    );
    send(&mut ui, "and now?");
    settle(&mut ui).await;
    assert_eq!(first.requests().len(), 1);
    let request = big.requests().pop().unwrap();
    assert_eq!(request.model, "big");
    // The conversation came along.
    assert!(request.messages.contains(&Message::User {
        content: "hello".into()
    }));
    assert!(shows(&ui, "from the big one"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn model_with_an_id_switches_and_the_window_follows() {
    let (first, _big, host) = two_models();
    let (mut ui, log, _dir) = open(first, host);
    send(&mut ui, "/model mock/big");
    settle(&mut ui).await;
    assert!(log.lock().unwrap().is_empty());
    assert!(
        status(&ui).starts_with("mock/big · auto · 0% of context"),
        "{}",
        status(&ui)
    );
    send(&mut ui, "/context");
    assert!(shows(
        &ui,
        "Context window: 200,000 tokens (from the model's profile)"
    ));
    send(&mut ui, "/model mock/big");
    assert!(shows(&ui, "already on mock/big"));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_model_that_cannot_be_used_leaves_the_session_as_it_was() {
    let (first, _big, host) = two_models();
    let (mut ui, _log, _dir) = open(first.clone(), host);
    send(&mut ui, "/model nope/x");
    settle(&mut ui).await;
    assert!(shows(
        &ui,
        "error: could not switch to nope/x: unknown provider `nope`"
    ));
    assert!(status(&ui).starts_with("mock/m · auto"), "{}", status(&ui));
    send(&mut ui, "still here?");
    settle(&mut ui).await;
    assert_eq!(first.requests().len(), 1);
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_switch_error_is_shown_with_secrets_replaced() {
    let (first, _big, host) = two_models();
    let dir = tempfile::tempdir().unwrap();
    let redactor = Arc::new(harness_core::redact::Redactor::default());
    redactor.add("sk-leaky-secret-123456");
    let (ui, _log) = start(
        agent(first, dir.path(), Mode::Auto),
        Box::new(host),
        options(dir.path(), Mode::Auto),
    );
    let mut ui = ui.with_redactor(redactor);
    send(&mut ui, "/model sk-leaky-secret-123456/x");
    settle(&mut ui).await;
    assert!(shows(&ui, "could not switch to"), "{:#?}", screen(&ui));
    assert!(
        !screen(&ui)
            .iter()
            .any(|r| r.starts_with("error:") && r.contains("sk-leaky-secret")),
        "{:#?}",
        screen(&ui)
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_stops_a_switch_that_waits() {
    let (first, _big, mut host) = two_models();
    host.stuck = true;
    let (mut ui, _log, _dir) = open(first, host);
    send(&mut ui, "/model mock/big");
    assert!(ui.app().busy());
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.contains("switching to mock/big… (Esc to stop)")),
        "{:#?}",
        screen(&ui)
    );
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(shows(&ui, "error: could not switch to mock/big: stopped"));
    assert!(status(&ui).starts_with("mock/m · auto"), "{}", status(&ui));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn the_picker_says_how_to_get_models_when_there_are_none() {
    let first = MockProvider::new(Vec::new());
    let (mut ui, _log, _dir) = open(first, Models::default());
    send(&mut ui, "/model");
    until(&mut ui, |app| {
        app.picker()
            .is_some_and(|p| p.items().is_empty() && !p.is_loading())
    })
    .await;
    ui.draw().unwrap();
    assert!(
        screen(&ui).iter().any(|r| r.contains("no models found")),
        "{:#?}",
        screen(&ui)
    );
    // Review B I1: and how to use a ChatGPT plan: sign in, or name its model.
    let advice = screen(&ui).join(" ");
    let advice = advice.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(advice.contains("/login"), "{advice}");
    assert!(advice.contains("/model chatgpt/<model>"), "{advice}");
    until_armed(&ui).await;
    press(&mut ui, KeyCode::Esc);
    assert!(ui.app().picker().is_none());
    ui.finish().await.unwrap();
}

// Review B I1: a model of a ChatGPT plan is marked as one in the picker, with the current one.
#[tokio::test]
async fn chatgpt_models_are_marked_as_such_in_the_picker() {
    let first = MockProvider::new(Vec::new());
    let big = MockProvider::new(Vec::new());
    let mut host = Models::default();
    host.models.insert("mock/m".into(), (first.clone(), 32_768));
    host.models
        .insert("chatgpt/gpt-5-codex".into(), (big, 272_000));
    let (mut ui, _log, _dir) = open(first, host);
    send(&mut ui, "/model");
    until(&mut ui, |app| {
        app.picker().is_some_and(|p| p.items().len() == 2)
    })
    .await;
    let items = ui.app().picker().unwrap().items().to_vec();
    let chatgpt = items.iter().find(|i| i.label == "chatgpt/gpt-5-codex");
    assert_eq!(chatgpt.map(|i| i.detail.as_str()), Some("ChatGPT plan"));
    let current = items.iter().find(|i| i.label == "mock/m");
    assert_eq!(current.map(|i| i.detail.as_str()), Some("(current)"));
    ui.finish().await.unwrap();
}

fn http(status: u16) -> Script {
    Script::error(harness_core::provider::ProviderError::Http {
        status,
        body: "no such model".into(),
        retry_after: None,
    })
}

// Final review minor 1: nothing can validate a model id offline, so the first run's choice is
// saved (by the host) only once the model has answered.
#[tokio::test]
async fn the_first_runs_model_is_reported_once_it_has_answered() {
    let first = MockProvider::new(vec![Script::text("hello there")]);
    let host = Models::default();
    let answered = host.answered.clone();
    let (mut ui, _log, _dir) = open(first, host);
    ui.app_mut().set_first_run_model();
    assert!(answered.lock().unwrap().is_empty());
    send(&mut ui, "hi");
    settle(&mut ui).await;
    assert_eq!(*answered.lock().unwrap(), ["mock/m"]);
    assert!(shows(&ui, "saved mock/m as your default model"));
    send(&mut ui, "again");
    settle(&mut ui).await;
    assert_eq!(answered.lock().unwrap().len(), 1);
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_first_run_model_that_fails_is_not_saved_and_the_user_is_told_to_pick_again() {
    let first = MockProvider::new(vec![http(404)]);
    let host = Models::default();
    let answered = host.answered.clone();
    let (mut ui, _log, _dir) = open(first, host);
    ui.app_mut().set_first_run_model();
    send(&mut ui, "hi");
    settle(&mut ui).await;
    assert!(answered.lock().unwrap().is_empty());
    assert!(
        shows(
            &ui,
            "mock/m did not answer, so it is not saved as your default; /model picks another"
        ),
        "{:#?}",
        screen(&ui)
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_switched_to_model_whose_request_is_refused_says_which_and_offers_the_way_back() {
    let (first, big, host) = two_models();
    drop(big);
    let big = MockProvider::new(vec![http(400)]);
    let mut host = host;
    host.models.insert("mock/big".into(), (big, 200_000));
    let (mut ui, _log, _dir) = open(first, host);
    send(&mut ui, "/model mock/big");
    settle(&mut ui).await;
    send(&mut ui, "hi");
    settle(&mut ui).await;
    assert!(
        shows(
            &ui,
            "mock/big did not accept the request; /model mock/m switches back"
        ),
        "{:#?}",
        screen(&ui)
    );
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn a_network_error_does_not_blame_the_model() {
    let first = MockProvider::new(vec![Script::error(
        harness_core::provider::ProviderError::Network("down".into()),
    )]);
    let host = Models::default();
    let (mut ui, _log, _dir) = open(first, host);
    ui.app_mut().set_first_run_model();
    send(&mut ui, "hi");
    settle(&mut ui).await;
    assert!(!shows(&ui, "did not answer"), "{:#?}", screen(&ui));
    ui.finish().await.unwrap();
}
