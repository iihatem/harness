//! `/model --role <role> <id>`: sets the model of one role for the session, checked with the
//! host first; `/model <id>` keeps setting `main`.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    permission::Mode,
    provider::ProviderEvent,
    testing::{MockProvider, Script},
};
use harness_tui::app::{Host, Prepared};
use ratatui::backend::TestBackend;
use tokio_util::sync::CancellationToken;

type Ui = harness_tui::ui::Ui<TestBackend>;

#[derive(Default)]
struct Calls {
    checked: Mutex<Vec<String>>,
    switched: Mutex<Vec<String>>,
}

struct Models {
    calls: Arc<Calls>,
    /// Ids the host cannot use, with why.
    refused: Vec<(&'static str, &'static str)>,
}

impl Host for Models {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn check_model(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<(), String>> {
        self.calls.checked.lock().unwrap().push(id.to_string());
        let refused = self
            .refused
            .iter()
            .find(|(refused, _)| *refused == id)
            .map(|(_, why)| why.to_string());
        Box::pin(async move { refused.map_or(Ok(()), Err) })
    }
    fn switch_model(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<harness_tui::app::ModelSwitch, String>> {
        self.calls.switched.lock().unwrap().push(id.to_string());
        Box::pin(async { Err("not in this test".to_string()) })
    }
}

fn open(
    script: Vec<Script>,
    refused: Vec<(&'static str, &'static str)>,
) -> (tempfile::TempDir, Ui, Arc<Calls>) {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(Calls::default());
    let (ui, _) = start(
        agent(MockProvider::new(script), dir.path(), Mode::Auto),
        Box::new(Models {
            calls: calls.clone(),
            refused,
        }),
        options(dir.path(), Mode::Auto),
    );
    (dir, ui, calls)
}

async fn wait_for(ui: &mut Ui, text: &str) {
    for _ in 0..50 {
        if shows(ui, text) {
            return;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), ui.next()).await;
    }
    panic!("never showed {text:?}: {:#?}", everything(ui));
}

// Spec "Changing one role" and "Showing roles": the role is set for the session, `/roles` says
// so, and `main` is unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_role_is_set_for_the_session_and_roles_says_so() {
    let (_dir, mut ui, calls) = open(vec![], vec![]);
    send(&mut ui, "/model --role build ollama/qwen3-coder:30b");
    wait_for(
        &mut ui,
        "switched to ollama/qwen3-coder:30b (build role, from mock/m; you chose it)",
    )
    .await;
    settle(&mut ui).await;
    assert_eq!(*calls.checked.lock().unwrap(), ["ollama/qwen3-coder:30b"]);
    assert!(calls.switched.lock().unwrap().is_empty());
    send(&mut ui, "/roles");
    assert!(
        shows(&ui, "  build       ollama/qwen3-coder:30b  session"),
        "{:#?}",
        everything(&ui)
    );
    assert!(shows(&ui, "  main        mock/m"), "{:#?}", everything(&ui));
}

// Spec "Unknown role": an error lists `main`, `plan`, `build` and `background`, and the roles
// are unchanged.
#[tokio::test]
async fn an_unknown_role_is_refused_with_the_list() {
    let (_dir, mut ui, calls) = open(vec![], vec![]);
    send(&mut ui, "/model --role review ollama/llama3");
    assert!(
        shows_wrapped(
            &ui,
            "unknown role `review`; the roles are main, plan, build and background"
        ),
        "{:#?}",
        everything(&ui)
    );
    assert!(calls.checked.lock().unwrap().is_empty());
    send(&mut ui, "/roles");
    assert!(!shows(&ui, "session"), "{:#?}", everything(&ui));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_the_host_cannot_use_leaves_the_role_as_it_was() {
    let (_dir, mut ui, _calls) = open(vec![], vec![("openai/gpt-5", "no API key for openai")]);
    send(&mut ui, "/model --role plan openai/gpt-5");
    wait_for(&mut ui, "no API key for openai").await;
    settle(&mut ui).await;
    send(&mut ui, "/roles");
    assert!(
        shows(&ui, "  plan        mock/m  inherited from main"),
        "{:#?}",
        everything(&ui)
    );
}

#[tokio::test]
async fn a_role_needs_its_model_too() {
    let (_dir, mut ui, calls) = open(vec![], vec![]);
    for typed in ["/model --role", "/model --role build"] {
        send(&mut ui, typed);
        assert!(
            shows(&ui, "/model --role takes a role and a model"),
            "{typed}: {:#?}",
            everything(&ui)
        );
    }
    send(&mut ui, "/model --role build a/b c/d");
    assert!(shows(&ui, "/model --role takes a role and a model"));
    assert!(calls.checked.lock().unwrap().is_empty());
}

// `/model --role main <id>` is `/model <id>`.
#[tokio::test(flavor = "multi_thread")]
async fn the_main_role_is_the_session_model() {
    let (_dir, mut ui, calls) = open(vec![], vec![]);
    send(&mut ui, "/model --role main ollama/llama3");
    wait_for(&mut ui, "not in this test").await;
    assert_eq!(*calls.switched.lock().unwrap(), ["ollama/llama3"]);
    assert!(calls.checked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn setting_a_role_waits_for_the_turn() {
    let (_dir, mut ui, calls) = open(
        vec![Script::Hang(vec![ProviderEvent::TextDelta(
            "thinking".into(),
        )])],
        vec![],
    );
    send(&mut ui, "a long task");
    until(&mut ui, |app| app.busy()).await;
    send(&mut ui, "/model --role build ollama/llama3");
    assert!(
        shows(&ui, "works between turns: press Esc to stop this one"),
        "{:#?}",
        everything(&ui)
    );
    assert!(calls.checked.lock().unwrap().is_empty());
}

// Review B minor 2: only `--role` is the option; `--roles` is not it with an `s` left over.
#[tokio::test]
async fn an_option_that_only_starts_like_role_is_not_role() {
    let (_dir, mut ui, calls) = open(vec![], vec![]);
    send(&mut ui, "/model --roles build ollama/llama3");
    assert!(
        shows(&ui, "unknown option `--roles`"),
        "{:#?}",
        everything(&ui)
    );
    assert!(!shows(&ui, "unknown role"), "{:#?}", everything(&ui));
    assert!(calls.checked.lock().unwrap().is_empty());
    assert!(calls.switched.lock().unwrap().is_empty());
}

// Review B minor 3: a role already on the model says so, as `/model` does, and announces no
// switch from a model to itself.
#[tokio::test]
async fn a_role_already_on_the_model_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(Calls::default());
    let (mut ui, _) = start(
        agent(MockProvider::new(vec![]), dir.path(), Mode::Auto).with_roles(
            harness_core::role::RoleConfig {
                build: Some("ollama/llama3".into()),
                ..Default::default()
            },
        ),
        Box::new(Models {
            calls: calls.clone(),
            refused: vec![],
        }),
        options(dir.path(), Mode::Auto),
    );
    send(&mut ui, "/model --role build ollama/llama3");
    assert!(
        shows(&ui, "build already runs on ollama/llama3"),
        "{:#?}",
        everything(&ui)
    );
    assert!(!shows(&ui, "switched to"), "{:#?}", everything(&ui));
    assert!(calls.checked.lock().unwrap().is_empty());
}

// Review B minor 4: Esc while the model is checked is a stop, shown as a note, not an error.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_check_of_a_role_model_is_a_note_not_an_error() {
    let (_dir, mut ui, _calls) = open(vec![], vec![("ollama/llama3", "stopped")]);
    send(&mut ui, "/model --role build ollama/llama3");
    wait_for(
        &mut ui,
        "stopped checking ollama/llama3; the build role is unchanged",
    )
    .await;
    assert!(!shows(&ui, "could not set"), "{:#?}", everything(&ui));
}
