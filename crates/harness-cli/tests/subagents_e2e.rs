//! M3b: the model starts two subagents in one tool call; they run at the same time, their events
//! carry their ids, and their reports reach the parent. Every model is a mock HTTP server.

mod common;
use common::Isolate;

use std::time::Duration;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_chunk(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}, "finish_reason": "stop"}]})
}

fn tool_chunk(id: &str, name: &str, arguments: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}}]}, "finish_reason": "tool_calls"}]})
}

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
}

struct Env {
    home: TempDir,
    ws: TempDir,
}

impl Env {
    fn new(server_uri: &str, extra_config: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        let agents = ws.path().join(".harness/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("alpha.md"),
            "---\ndescription: Looks at the first thing\ntools: Read, Grep\n---\nALPHA-BODY-MARKER: look at the first thing.\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("beta.md"),
            "---\ndescription: Does the second thing\n---\nBETA-BODY-MARKER: do the second thing.\n",
        )
        .unwrap();
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        // A hang fails the test instead of holding it.
        cmd.timeout(Duration::from_secs(60))
            .current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

/// The parent asks for both agents; each child answers after a pause (so they overlap if they
/// run together); the parent's next request, which holds the reports, gets the final answer.
async fn script(server: &MockServer) {
    let url = || path("/v1/chat/completions");
    let slow = |text: &str| stream(&[text_chunk(text)]).set_delay(Duration::from_millis(600));
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("ALPHA-BODY-MARKER"))
        .respond_with(slow("alpha found three files"))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("BETA-BODY-MARKER"))
        .respond_with(slow("beta found nothing"))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("Both subagents reported.")]))
        .with_priority(2)
        .mount(server)
        .await;
    let args = json!({"agents": [
        {"agent": "alpha", "task": "TASK-ONE: inspect the first thing"},
        {"agent": "beta", "task": "TASK-TWO: do the second thing"},
    ]});
    Mock::given(method("POST"))
        .and(url())
        .respond_with(stream(&[tool_chunk(
            "c1",
            "spawn_agents",
            &args.to_string(),
        )]))
        .with_priority(3)
        .mount(server)
        .await;
}

fn events(stdout: Vec<u8>) -> Vec<Value> {
    String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("every stdout line is JSON"))
        .collect()
}

fn position(events: &[Value], what: impl Fn(&Value) -> bool) -> usize {
    events
        .iter()
        .position(what)
        .unwrap_or_else(|| panic!("no such event in {events:#?}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn two_subagents_run_together_and_their_reports_reach_the_parent() {
    let server = MockServer::start().await;
    script(&server).await;
    // The mock server is on this machine, so it counts as local: allow two at once there.
    let env = Env::new(
        &server.uri(),
        "model = \"mock/test-model\"\n[agents]\nper_local = 2\n",
    );
    let output = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "--json", "go"]).output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = events(output.stdout);

    // The main agent's own events carry no `agent`; the children's carry theirs.
    let kind = |e: &Value, t: &str| e["type"] == t;
    let started: Vec<&Value> = events.iter().filter(|e| kind(e, "agent_started")).collect();
    assert_eq!(started.len(), 2, "{events:#?}");
    let mut ids: Vec<&str> = started
        .iter()
        .map(|e| e["agent"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, ["a1", "a2"]);
    let labels: Vec<&str> = {
        let mut l: Vec<&str> = started
            .iter()
            .map(|e| e["label"].as_str().unwrap())
            .collect();
        l.sort();
        l
    };
    assert_eq!(labels, ["alpha", "beta"]);

    // They ran at the same time: both started before either finished.
    let last_start = events
        .iter()
        .rposition(|e| kind(e, "agent_started"))
        .unwrap();
    let first_finish = position(&events, |e| kind(e, "agent_finished"));
    assert!(
        last_start < first_finish,
        "the children ran one after another"
    );

    // Their reports came back to the parent as the tool's result, in the order asked.
    let result = events
        .iter()
        .find(|e| kind(e, "tool_call_finished") && e.get("agent").is_none())
        .expect("the parent's tool call finished");
    assert_eq!(result["is_error"], false, "{result}");
    let output = result["output"].as_str().unwrap();
    assert!(output.contains("alpha found three files"), "{output}");
    assert!(output.contains("beta found nothing"), "{output}");
    assert!(
        output.find("agent=\"alpha\"").unwrap() < output.find("agent=\"beta\"").unwrap(),
        "{output}"
    );
    assert_eq!(output.matches("status=\"done\"").count(), 2, "{output}");
    // Neither called a tool, and the parent is told.
    assert_eq!(
        output.matches("without calling any tool").count(),
        2,
        "{output}"
    );

    // The parent's turn is the last event, and it completed with the parent's own answer.
    let last = events.last().unwrap();
    assert_eq!(last["type"], "turn_finished");
    assert!(last.get("agent").is_none());
    assert_eq!(last["reason"], "completed");
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "assistant_message" && e["content"] == "Both subagents reported.")
    );

    // What each child was sent: its task and its definition's instructions, not the parent's
    // conversation.
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    let alpha = bodies
        .iter()
        .find(|b| b.contains("ALPHA-BODY-MARKER"))
        .unwrap();
    assert!(alpha.contains("TASK-ONE"), "{alpha}");
    assert!(
        !alpha.contains("TASK-TWO"),
        "a child does not see its sibling's task"
    );
    assert!(
        !alpha.contains("\"content\":\"go\""),
        "a child does not see the parent's conversation"
    );
    // The check above means something only if the parent's own request carries that text.
    assert!(
        bodies.iter().any(|b| b.contains("\"content\":\"go\"")),
        "the parent's prompt is not sent as {{\"content\":\"go\"}}, so the check above is empty"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_agent_runs_on_the_background_role_and_an_editing_one_on_the_parents_model() {
    let server = MockServer::start().await;
    script(&server).await;
    let env = Env::new(
        &server.uri(),
        "model = \"mock/test-model\"\n[agents]\nper_local = 2\n[roles]\nbackground = \"mock/small-model\"\n",
    );
    let output = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "--json", "go"]).output().unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success());
    let requests = server.received_requests().await.unwrap();
    let model_of = |marker: &str| -> String {
        let body = requests
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .find(|b| b.contains(marker))
            .unwrap();
        serde_json::from_str::<Value>(&body).unwrap()["model"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // `alpha` lists only read tools: it is read-only. `beta` lists none: it can edit.
    assert_eq!(model_of("ALPHA-BODY-MARKER"), "small-model");
    assert_eq!(model_of("BETA-BODY-MARKER"), "test-model");
}

/// Runs `ask --json go` against a server that answers the first request with `reply` and any
/// request holding a tool result with a final text.
async fn run_with(
    config: &str,
    project_config: Option<&str>,
    reply: Value,
) -> (Vec<Value>, Vec<String>) {
    let server = MockServer::start().await;
    let url = || path("/v1/chat/completions");
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("done")]))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(url())
        .respond_with(stream(&[reply]))
        .with_priority(2)
        .mount(&server)
        .await;
    let env = Env::new(&server.uri(), config);
    if let Some(project) = project_config {
        std::fs::write(env.ws.path().join(".harness/config.toml"), project).unwrap();
    }
    let output = tokio::task::spawn_blocking(move || {
        env.cmd().args(["ask", "--json", "go"]).output().unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bodies = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    (events(output.stdout), bodies)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_main_agent_is_offered_the_tool_unless_spawn_is_off_in_the_global_or_project_config() {
    let reply = text_chunk("hi");
    let (_, on) = run_with("model = \"mock/test-model\"\n", None, reply.clone()).await;
    assert!(on[0].contains("spawn_agents"), "{}", on[0]);
    let (_, global) = run_with(
        "model = \"mock/test-model\"\n[agents]\nspawn = false\n",
        None,
        reply.clone(),
    )
    .await;
    assert!(!global[0].contains("spawn_agents"), "{}", global[0]);
    let (_, project) = run_with(
        "model = \"mock/test-model\"\n",
        Some("[agents]\nspawn = false\n"),
        reply.clone(),
    )
    .await;
    assert!(!project[0].contains("spawn_agents"), "{}", project[0]);
    // A project cannot turn it back on.
    let (_, cannot) = run_with(
        "model = \"mock/test-model\"\n[agents]\nspawn = false\n",
        Some("[agents]\nspawn = true\n"),
        reply,
    )
    .await;
    assert!(!cannot[0].contains("spawn_agents"), "{}", cannot[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spawn_entry_naming_a_model_nobody_chose_is_refused_with_the_models_it_may_name() {
    let args = json!({"agents": [
        {"agent": "alpha", "task": "t", "model": "mock/not-in-play"},
    ]});
    let (events, bodies) = run_with(
        "model = \"mock/test-model\"\n",
        None,
        tool_chunk("c1", "spawn_agents", &args.to_string()),
    )
    .await;
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_call_finished")
        .expect("the tool call finished");
    assert_eq!(result["is_error"], true, "{result}");
    let output = result["output"].as_str().unwrap();
    assert!(output.contains("mock/not-in-play"), "{output}");
    assert!(output.contains("mock/test-model"), "{output}");
    assert!(
        !events.iter().any(|e| e["type"] == "agent_started"),
        "{events:#?}"
    );
    assert_eq!(bodies.len(), 2, "no child made a request");
}
