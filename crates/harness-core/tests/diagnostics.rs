//! Diagnostics after an edit: what a language-server front end says is appended to the edit
//! result, after the lint output.

mod common;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use common::{
    finished_outputs,
    gates::{ScriptedBash, Setup, agent, failing},
    run,
};
use harness_core::{
    diag::{Diagnostics, EditedFiles},
    gate::Gates,
    permission::{FsAccess, Mode},
    session::Session,
    testing::{MockProvider, Script},
};
use serde_json::json;

/// The paths, the access, whether there was a sandbox, and whether running without one was allowed.
type Seen = (Vec<PathBuf>, FsAccess, bool, bool);

#[derive(Default)]
struct Fake {
    answer: Mutex<Option<String>>,
    seen: Mutex<Vec<Seen>>,
    resets: Mutex<u32>,
    shutdowns: Mutex<u32>,
}

#[async_trait]
impl Diagnostics for Fake {
    async fn after_edit(&self, edited: &EditedFiles<'_>) -> Option<String> {
        self.seen.lock().unwrap().push((
            edited.paths.to_vec(),
            edited.access,
            edited.sandbox.is_some(),
            edited.unsandboxed_ok,
        ));
        self.answer.lock().unwrap().clone()
    }
    fn reset(&self) {
        *self.resets.lock().unwrap() += 1;
    }
    async fn shutdown(&self) {
        *self.shutdowns.lock().unwrap() += 1;
    }
}

fn edits() -> Arc<MockProvider> {
    MockProvider::new(vec![
        Script::tool_call(
            "e1",
            "edit",
            json!({"path": "a.ts", "content": "let x: number = 'a';\n"}),
        ),
        Script::text("ok"),
    ])
}

#[tokio::test]
async fn what_the_diagnostics_say_is_appended_to_the_edit_result() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.answer.lock().unwrap() = Some("[diagnostics: 1 error]\na.ts:1: bad".into());
    let (bash, _) = ScriptedBash::new(vec![]);
    let mut agent =
        agent(edits(), dir.path(), bash, Setup::default()).with_diagnostics(fake.clone());
    let (_, events) = run(&mut agent, "edit").await;
    assert_eq!(
        finished_outputs(&events)[0].0,
        "edited\n[diagnostics: 1 error]\na.ts:1: bad"
    );
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, [dir.path().canonicalize().unwrap().join("a.ts")]);
}

// Nothing to say: the edit result is as it was.
#[tokio::test]
async fn silence_leaves_the_edit_result_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let (bash, _) = ScriptedBash::new(vec![]);
    let mut agent = agent(edits(), dir.path(), bash, Setup::default()).with_diagnostics(fake);
    let (_, events) = run(&mut agent, "edit").await;
    assert_eq!(finished_outputs(&events)[0].0, "edited");
}

// The lint output comes first, then the diagnostics: both in the same result.
#[tokio::test]
async fn diagnostics_follow_the_lint_output() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.answer.lock().unwrap() = Some("[diagnostics: 1 error]".into());
    let (bash, _) = ScriptedBash::new(vec![("lint", vec![failing(1, "lint says no\n")])]);
    let mut agent = agent(
        edits(),
        dir.path(),
        bash,
        Setup {
            gates: Gates {
                after_edit: Some("lint".into()),
                ..Gates::default()
            },
            ..Setup::default()
        },
    )
    .with_diagnostics(fake);
    let (_, events) = run(&mut agent, "edit").await;
    let result = &finished_outputs(&events)[0].0;
    let lint = result.find("lint says no").unwrap();
    let diagnostics = result.find("[diagnostics: 1 error]").unwrap();
    assert!(lint < diagnostics, "{result}");
}

#[tokio::test]
async fn a_failed_edit_asks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let (bash, _) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![
        Script::tool_call("e1", "edit", json!({"path": "a.ts", "content": "FAIL"})),
        Script::text("ok"),
    ]);
    let mut agent =
        agent(provider, dir.path(), bash, Setup::default()).with_diagnostics(fake.clone());
    run(&mut agent, "edit").await;
    assert!(fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn read_only_modes_ask_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let (bash, _) = ScriptedBash::new(vec![]);
    let provider = MockProvider::new(vec![
        Script::tool_call("s1", "sneaky", json!({})),
        Script::text("ok"),
    ]);
    let mut agent = agent(
        provider,
        dir.path(),
        bash,
        Setup {
            mode: Mode::Plan,
            ..Setup::default()
        },
    )
    .with_diagnostics(fake.clone());
    run(&mut agent, "go").await;
    assert!(fake.seen.lock().unwrap().is_empty());
}

// Servers run only where bash does: in full-access they may run without a sandbox, in other
// modes they do not.
#[tokio::test]
async fn full_access_is_the_only_mode_that_lets_a_server_run_without_a_sandbox() {
    for (mode, expected) in [(Mode::Auto, false), (Mode::FullAccess, true)] {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(Fake::default());
        let (bash, _) = ScriptedBash::new(vec![]);
        let mut agent = agent(
            edits(),
            dir.path(),
            bash,
            Setup {
                mode,
                ..Setup::default()
            },
        )
        .with_diagnostics(fake.clone());
        run(&mut agent, "edit").await;
        let seen = fake.seen.lock().unwrap().clone();
        assert_eq!(seen[0].3, expected, "{mode}");
        assert_eq!(seen[0].1, mode.fs_access());
    }
}

// Spec "New session": every running language server is stopped on `/new`.
#[tokio::test]
async fn a_new_session_resets_the_servers_and_closing_shuts_them_down() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    let (bash, _) = ScriptedBash::new(vec![]);
    let mut agent =
        agent(edits(), dir.path(), bash, Setup::default()).with_diagnostics(fake.clone());
    agent.start_session(Session::in_memory(dir.path()), None);
    assert_eq!(*fake.resets.lock().unwrap(), 1);
    agent.close().await;
    assert_eq!(*fake.shutdowns.lock().unwrap(), 1);
}
