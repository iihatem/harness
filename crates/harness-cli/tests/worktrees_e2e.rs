//! M3d: a subagent that edits works in a worktree of its own; headless `ask` reports its branch
//! and never applies it. Every model is a mock HTTP server; git is real.

mod common;
use common::Isolate;

use std::time::Duration;

use assert_cmd::Command;
use harness_core::worktree::testing::{git, init_repo};
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
    fn new(server_uri: &str) -> Env {
        let home = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        init_repo(ws.path());
        let agents = ws.path().join(".harness/agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("builder.md"),
            "---\ndescription: Builds a thing\nisolation: worktree\n---\nBUILDER-BODY-MARKER: build the thing.\n",
        )
        .unwrap();
        git(ws.path(), &["add", "-A"]);
        git(ws.path(), &["commit", "-q", "-m", "agents"]);
        Env { home, ws }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
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

/// The parent starts `builder`; the builder writes `src/built.rs` and says it is done; the parent
/// answers once it has the report.
async fn script(server: &MockServer) {
    let url = || path("/v1/chat/completions");
    let write = json!({"path": "src/built.rs", "content": "pub fn built() {}\n"});
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("BUILDER-BODY-MARKER"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("built it")]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("BUILDER-BODY-MARKER"))
        .respond_with(stream(&[tool_chunk("w1", "write", &write.to_string())]))
        .with_priority(2)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("The builder reported.")]))
        .with_priority(3)
        .mount(server)
        .await;
    let args = json!({"agents": [{"agent": "builder", "task": "build the thing"}]});
    Mock::given(method("POST"))
        .and(url())
        .respond_with(stream(&[tool_chunk(
            "c1",
            "spawn_agents",
            &args.to_string(),
        )]))
        .with_priority(4)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_builder_works_in_a_worktree_and_ask_reports_its_branch_without_applying_it() {
    let server = MockServer::start().await;
    script(&server).await;
    let env = Env::new(&server.uri());
    // Uncommitted work in the main checkout: not in the worktree, and said once.
    std::fs::write(env.ws.path().join("README.md"), "edited, not committed\n").unwrap();
    let (ws, home) = (env.ws.path().to_path_buf(), env.home.path().to_path_buf());
    let output = tokio::task::spawn_blocking(move || {
        let json = env
            .cmd()
            .args(["--mode", "auto", "ask", "--json", "go"])
            .output()
            .unwrap();
        let text = env
            .cmd()
            .args(["--mode", "auto", "ask", "go"])
            .output()
            .unwrap();
        (json, text, env)
    })
    .await
    .unwrap();
    let (json_out, text_out, _env) = output;
    assert!(
        json_out.status.success(),
        "{}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    let events: Vec<Value> = String::from_utf8(json_out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let finished = events
        .iter()
        .find(|e| e["type"] == "agent_finished")
        .expect("the builder finished");
    assert_eq!(finished["status"], "done", "{finished}");
    let branch = finished["branch"]
        .as_str()
        .expect("its branch is reported")
        .to_string();
    assert!(branch.starts_with("harness/builder"), "{branch}");
    let warned: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "warning" && e.get("agent").is_none())
        .collect();
    assert!(
        warned
            .iter()
            .any(|w| w["message"].as_str().unwrap().contains("not committed")),
        "{warned:?}"
    );
    // The child's tool result carries the branch for the parent too.
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_call_finished" && e.get("agent").is_none())
        .unwrap();
    assert!(
        result["output"]
            .as_str()
            .unwrap()
            .contains("branch: harness/builder"),
        "{result}"
    );

    // Headless never applies: the main checkout has its own edit and no new file; the branch
    // holds the commit; the worktree directory is under harness's data directory.
    assert!(!ws.join("src/built.rs").exists());
    assert_eq!(
        std::fs::read_to_string(ws.join("README.md")).unwrap(),
        "edited, not committed\n"
    );
    let shown = git(&ws, &["show", &format!("{branch}:src/built.rs")]);
    assert_eq!(shown, "pub fn built() {}\n");
    let list = git(&ws, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains(home.to_str().unwrap())
            || list.contains(&home.canonicalize().unwrap().to_string_lossy().to_string()),
        "{list}"
    );
    assert!(!list.contains("locked"), "the lock is released: {list}");

    // The text run names the branch on stderr.
    assert!(text_out.status.success());
    let stderr = String::from_utf8_lossy(&text_out.stderr);
    assert!(stderr.contains("harness/builder"), "{stderr}");
    assert!(stderr.contains("not applied"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_run_says_why_a_child_failed_not_only_that_it_did() {
    let server = MockServer::start().await;
    script(&server).await;
    let env = Env::new(&server.uri());
    let out = tokio::task::spawn_blocking(move || {
        // In `ask` mode the builder's write needs an approval nobody can give.
        let out = env
            .cmd()
            .args(["--mode", "ask", "ask", "go"])
            .output()
            .unwrap();
        (out, env)
    })
    .await
    .unwrap()
    .0;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("agent a1 failed"), "{stderr}");
    assert!(
        stderr.contains("agent a1 blocked: write src/built.rs"),
        "{stderr}"
    );
}
