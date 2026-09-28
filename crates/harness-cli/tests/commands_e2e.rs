use assert_cmd::Command;
use harness_context::commands::frontmatter;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");
/// This repository's own OpenSpec command, as `openspec init` wrote it.
const OPSX_PROPOSE: &str = include_str!("../../../.claude/commands/opsx/propose.md");

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

/// A HARNESS_HOME whose config uses provider `mock` at `server_uri` (plus `extra_config`), and a
/// workspace that looks like a git work tree.
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
            format!("model = \"mock/test-model\"\n{extra_config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        Env { home, ws }
    }

    fn command_file(&self, name: &str, text: &str) {
        let path = self.ws.path().join(".claude/commands").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME");
        cmd
    }
}

/// Answers every request with `text`.
async fn answer(server: &MockServer, text: &str) {
    Mock::given(method("POST"))
        .respond_with(stream(&[text_chunk(text)]))
        .mount(server)
        .await;
}

/// First requests `tool` with `arguments`, then answers `done` once the tool result is in.
async fn tool_then_answer(server: &MockServer, tool: &str, arguments: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[text_chunk("done")]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(stream(&[tool_chunk("c1", tool, arguments)]))
        .with_priority(2)
        .mount(server)
        .await;
}

/// The user message of each request the server received.
async fn user_messages(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["messages"][1]["content"].as_str().unwrap().to_string()
        })
        .collect()
}

fn run(env: Env, args: &[&str]) -> std::process::Output {
    env.cmd().args(args).output().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn opsx_propose_expands_from_claude_commands() {
    let server = MockServer::start().await;
    answer(&server, "Created the change.").await;
    let env = Env::new(&server.uri(), "");
    env.command_file("opsx/propose.md", OPSX_PROPOSE);
    let output = tokio::task::spawn_blocking(move || run(env, &["ask", "/opsx:propose add-login"]))
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("Created the change."));
    let (_, body) = frontmatter::parse(OPSX_PROPOSE);
    assert_eq!(
        user_messages(&server).await,
        [format!("{}\n\nARGUMENTS: add-login", body.trim_end())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_tools_preapprove_a_command_in_ask_mode() {
    let server = MockServer::start().await;
    tool_then_answer(&server, "bash", r#"{"command":"openspec status"}"#).await;
    let env = Env::new(&server.uri(), "mode = \"ask\"");
    env.command_file(
        "status.md",
        "---\nallowed-tools: Bash(openspec:*)\n---\nShow the status.\n",
    );
    let output = tokio::task::spawn_blocking(move || run(env, &["ask", "/status"]))
        .await
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("blocked"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_tools_cannot_beat_a_deny_rule() {
    let server = MockServer::start().await;
    tool_then_answer(&server, "bash", r#"{"command":"rm -rf build"}"#).await;
    let env = Env::new(&server.uri(), "[permissions]\ndeny = [\"bash:rm -rf*\"]");
    env.command_file("clean.md", "---\nallowed-tools: Bash(*)\n---\nClean up.\n");
    std::fs::create_dir(env.ws.path().join("build")).unwrap();
    let (env, output) = tokio::task::spawn_blocking(move || {
        let output = env
            .cmd()
            .args(["ask", "--json", "/clean"])
            .output()
            .unwrap();
        (env, output)
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(env.ws.path().join("build").exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("matches deny rule `bash:rm -rf*`"),
        "{stdout}"
    );
}

// Spec: a command's shell expansion goes through the same approval as the bash tool. Headless
// runs cannot approve, so the command is blocked and the model is told.
#[tokio::test(flavor = "multi_thread")]
async fn shell_expansion_in_ask_mode_needs_approval() {
    let server = MockServer::start().await;
    answer(&server, "I could not see the diff.").await;
    let env = Env::new(&server.uri(), "mode = \"ask\"");
    env.command_file("review.md", "Review this diff:\n!`git diff`\n");
    let output = tokio::task::spawn_blocking(move || run(env, &["ask", "/review"]))
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("blocked: run `git diff`"));
    let messages = user_messages(&server).await;
    assert!(
        messages[0].starts_with("Review this diff:\n[`git diff` did not run successfully]"),
        "{messages:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn piped_stdin_follows_the_expanded_command() {
    let server = MockServer::start().await;
    answer(&server, "ok").await;
    let env = Env::new(&server.uri(), "");
    env.command_file("review.md", "Review $1 carefully.\n");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "/review", "src/lib.rs"])
            .write_stdin("diff --git a/x b/x\n")
            .assert()
            .success();
    })
    .await
    .unwrap();
    assert_eq!(
        user_messages(&server).await,
        ["Review src/lib.rs carefully.\n\n\ndiff --git a/x b/x\n"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn builtins_other_than_init_are_rejected_headless() {
    let server = MockServer::start().await;
    answer(&server, "ok").await;
    let env = Env::new(&server.uri(), "");
    env.command_file("help.md", "mine\n");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "/help"])
            .assert()
            .code(2)
            .stderr(contains("/help is a built-in command"))
            .stderr(contains("error: /help works only in interactive mode"));
        env.cmd().args(["ask", "/compact"]).assert().code(2);
    })
    .await
    .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_command_is_rejected_but_a_path_is_ordinary_text() {
    let server = MockServer::start().await;
    answer(&server, "ok").await;
    let env = Env::new(&server.uri(), "");
    tokio::task::spawn_blocking(move || {
        env.cmd()
            .args(["ask", "/nope", "x"])
            .assert()
            .code(2)
            .stderr(contains("unknown command /nope"));
        env.cmd()
            .args(["ask", "/usr/bin/env is missing"])
            .assert()
            .success();
    })
    .await
    .unwrap();
    assert_eq!(user_messages(&server).await, ["/usr/bin/env is missing"]);
}

/// The model each request the server received asked for.
async fn requested_models(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            body["model"].as_str().unwrap().to_string()
        })
        .collect()
}

const PICKS_A_MODEL: &str = "---\nmodel: mock/command-model\n---\nGo.\n";

// Decision 5 (as changed): a repository's command file may pick the model only once the user
// trusted the workspace.
#[tokio::test(flavor = "multi_thread")]
async fn a_project_commands_model_applies_only_once_the_workspace_is_trusted() {
    let server = MockServer::start().await;
    answer(&server, "ok").await;
    let env = Env::new(&server.uri(), "");
    env.command_file("pick.md", PICKS_A_MODEL);
    std::fs::create_dir_all(env.ws.path().join(".harness")).unwrap();
    std::fs::write(
        env.ws.path().join(".harness/config.toml"),
        "[permissions]\nallow = [\"bash:make*\"]\n",
    )
    .unwrap();
    let (untrusted, trusted) = tokio::task::spawn_blocking(move || {
        let untrusted = env.cmd().args(["ask", "/pick"]).output().unwrap();
        env.cmd().args(["trust", "--yes"]).assert().success();
        let trusted = env.cmd().args(["ask", "/pick"]).output().unwrap();
        (untrusted, trusted)
    })
    .await
    .unwrap();
    assert!(untrusted.status.success(), "{untrusted:?}");
    let stderr = String::from_utf8_lossy(&untrusted.stderr);
    assert!(
        stderr.contains("note: /pick asks for model mock/command-model")
            && stderr.contains("trusted workspace"),
        "{stderr}"
    );
    assert!(trusted.status.success(), "{trusted:?}");
    let stderr = String::from_utf8_lossy(&trusted.stderr);
    assert!(
        stderr.contains("note: /pick runs on mock/command-model"),
        "{stderr}"
    );
    assert_eq!(
        requested_models(&server).await,
        ["test-model", "command-model"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_global_commands_model_applies_without_trust() {
    let server = MockServer::start().await;
    answer(&server, "ok").await;
    let env = Env::new(&server.uri(), "");
    let global = env.home.path().join("config/commands/pick.md");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, PICKS_A_MODEL).unwrap();
    let output = tokio::task::spawn_blocking(move || run(env, &["ask", "/pick"]))
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("note: /pick runs on mock/command-model")
    );
    assert_eq!(requested_models(&server).await, ["command-model"]);
}
