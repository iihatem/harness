//! `/login` inside the session: the host signs in, what it says while it waits is shown, and
//! Esc stops it.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use futures::future::BoxFuture;
use harness_core::{
    permission::Mode,
    provider::ProviderEvent,
    testing::{MockProvider, Script},
};
use harness_tui::{
    app::{Host, Prepared},
    ui::Ui,
};
use ratatui::{backend::TestBackend, crossterm::event::KeyCode};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Signs in to `chatgpt` after saying where, or waits until stopped; refuses anything else.
#[derive(Default)]
struct SignIn {
    /// The provider and whether a device code was asked for, per sign-in.
    asked: Arc<Mutex<Vec<(String, bool)>>>,
    waits: bool,
}

impl Host for SignIn {
    fn is_command(&self, _name: &str) -> bool {
        false
    }
    fn prepare(&mut self, _typed: &str) -> Prepared {
        unreachable!()
    }
    fn login(
        &self,
        provider: &str,
        device: bool,
        notes: mpsc::UnboundedSender<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String, String>> {
        self.asked
            .lock()
            .unwrap()
            .push((provider.to_string(), device));
        let provider = provider.to_string();
        let waits = self.waits;
        Box::pin(async move {
            if provider != "chatgpt" {
                return Err(format!("{provider} takes an API key, not a sign-in"));
            }
            let _ = notes.send(
                "To sign in, open https://auth.example/device and enter the code ABCD-1234".into(),
            );
            if waits {
                cancel.cancelled().await;
                return Err("sign-in cancelled".into());
            }
            Ok("Signed in to ChatGPT as dev@example.com (profile default).".into())
        })
    }
}

fn open(host: SignIn, provider: Arc<MockProvider>) -> (Ui<TestBackend>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (ui, _log) = start(
        agent(provider, dir.path(), Mode::Auto),
        Box::new(host),
        options(dir.path(), Mode::Auto),
    );
    (ui, dir)
}

#[tokio::test]
async fn login_signs_in_to_chatgpt_and_shows_what_it_says() {
    let host = SignIn::default();
    let asked = host.asked.clone();
    let (mut ui, _dir) = open(host, MockProvider::new(Vec::new()));
    send(&mut ui, "/login");
    settle(&mut ui).await;
    assert_eq!(*asked.lock().unwrap(), [("chatgpt".to_string(), false)]);
    assert!(shows(&ui, "enter the code ABCD-1234"));
    assert!(shows(
        &ui,
        "Signed in to ChatGPT as dev@example.com (profile default)."
    ));
    // What it says is true: the picker offers ChatGPT's models once signed in (the host lists
    // them), and any other is named.
    assert!(shows(&ui, "/model offers ChatGPT's models"));
    assert!(shows(&ui, "/model chatgpt/<model> uses any other"));
    assert!(!ui.app().busy());
    send(&mut ui, "/login chatgpt --device");
    settle(&mut ui).await;
    assert_eq!(asked.lock().unwrap()[1], ("chatgpt".to_string(), true));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn esc_stops_a_sign_in_that_waits() {
    let host = SignIn {
        waits: true,
        ..SignIn::default()
    };
    let (mut ui, _dir) = open(host, MockProvider::new(Vec::new()));
    send(&mut ui, "/login");
    until(&mut ui, |app| app.busy()).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !shows(&ui, "ABCD-1234") {
            ui.next().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.contains("signing in… (Esc to stop)")),
        "{:#?}",
        screen(&ui)
    );
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    assert!(shows(&ui, "error: could not sign in: sign-in cancelled"));
    assert!(!ui.app().busy());
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn what_the_host_refuses_is_shown_as_an_error() {
    let (mut ui, _dir) = open(SignIn::default(), MockProvider::new(Vec::new()));
    send(&mut ui, "/login openai");
    settle(&mut ui).await;
    assert!(shows(
        &ui,
        "error: could not sign in: openai takes an API key, not a sign-in"
    ));
    ui.finish().await.unwrap();
}

#[tokio::test]
async fn login_waits_for_the_turn() {
    let provider = MockProvider::new(vec![Script::Hang(vec![ProviderEvent::TextDelta(
        "working".into(),
    )])]);
    let host = SignIn::default();
    let asked = host.asked.clone();
    let (mut ui, _dir) = open(host, provider);
    send(&mut ui, "a long task");
    until(&mut ui, |app| app.busy()).await;
    send(&mut ui, "/login");
    assert!(
        screen(&ui)
            .iter()
            .any(|r| r.contains("/login works between turns")),
        "{:#?}",
        screen(&ui)
    );
    assert!(asked.lock().unwrap().is_empty());
    press(&mut ui, KeyCode::Esc);
    settle(&mut ui).await;
    ui.finish().await.unwrap();
}
