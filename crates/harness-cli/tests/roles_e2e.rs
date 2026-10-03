//! Roles through the real binary: a `plan` role sends planning to its own server, the session's
//! model is `[roles] main` when no `model` is set, and a cloned repository's roles are ignored
//! until trusted.

mod common;
use common::Isolate;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn answer(text: &str) -> ResponseTemplate {
    let chunk =
        json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]});
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

async fn server(text: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer(text))
        .mount(&server)
        .await;
    server
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    /// Providers `main` and `planner` at the two servers, with the top-level settings `top` first
    /// and the tables `tables` last.
    fn new(main: &MockServer, planner: &MockServer, top: &str, tables: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "{top}[providers.main]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[providers.planner]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[profiles.\"*\"]\ncontext_window = 32768\n{tables}",
                main.uri(),
                planner.uri()
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

async fn run(env: Env, args: &'static [&'static str]) -> (String, String) {
    let output = tokio::task::spawn_blocking(move || env.cmd().args(args).output().unwrap())
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

async fn bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.body_json().unwrap())
        .collect()
}

// Spec "Partial configuration": turns in `plan` mode run on the plan role's model and all other
// work on `main`.
#[tokio::test(flavor = "multi_thread")]
async fn a_plan_mode_run_goes_to_the_plan_role() {
    let (main, planner) = (server("from main").await, server("the plan").await);
    let env = Env::new(
        &main,
        &planner,
        "model = \"main/m\"\n",
        "[roles]\nplan = \"planner/p\"\n",
    );
    let (stdout, _) = run(env, &["--mode", "plan", "ask", "plan it"]).await;
    assert!(stdout.contains("the plan"), "{stdout}");
    assert!(bodies(&main).await.is_empty());
    let sent = bodies(&planner).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["model"], "p");
}

#[tokio::test(flavor = "multi_thread")]
async fn other_runs_stay_on_main_whatever_the_plan_role_is() {
    let (main, planner) = (server("from main").await, server("the plan").await);
    let env = Env::new(
        &main,
        &planner,
        "model = \"main/m\"\n",
        "[roles]\nplan = \"planner/p\"\n",
    );
    let (stdout, _) = run(env, &["--mode", "read-only", "ask", "look"]).await;
    assert!(stdout.contains("from main"), "{stdout}");
    assert!(bodies(&planner).await.is_empty());
}

// Spec "Defaults": with no roles, planning runs on the session's model.
#[tokio::test(flavor = "multi_thread")]
async fn without_roles_planning_runs_on_main() {
    let (main, planner) = (server("from main").await, server("the plan").await);
    let env = Env::new(&main, &planner, "model = \"main/m\"\n", "");
    let (stdout, _) = run(env, &["--mode", "plan", "ask", "plan it"]).await;
    assert!(stdout.contains("from main"), "{stdout}");
    assert!(bodies(&planner).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_main_is_the_model_when_no_model_is_set() {
    let (main, planner) = (server("from main").await, server("the plan").await);
    let env = Env::new(&main, &planner, "", "[roles]\nmain = \"main/m2\"\n");
    let (stdout, _) = run(env, &["ask", "hi"]).await;
    assert!(stdout.contains("from main"), "{stdout}");
    assert_eq!(bodies(&main).await[0]["model"], "m2");
}

// Spec "Untrusted project roles": ignored, with a notice, until the workspace is trusted.
#[tokio::test(flavor = "multi_thread")]
async fn a_cloned_repositorys_roles_are_ignored_until_trusted() {
    let (main, planner) = (server("from main").await, server("the plan").await);
    let env = Env::new(&main, &planner, "model = \"main/m\"\n", "");
    std::fs::create_dir_all(env.ws.path().join(".harness")).unwrap();
    std::fs::write(
        env.ws.path().join(".harness/config.toml"),
        "[roles]\nplan = \"planner/p\"\n",
    )
    .unwrap();
    let (stdout, stderr) = run(env, &["--mode", "plan", "ask", "plan it"]).await;
    assert!(stdout.contains("from main"), "{stdout}");
    assert!(stderr.contains("roles.plan"), "{stderr}");
    assert!(bodies(&planner).await.is_empty());
}
