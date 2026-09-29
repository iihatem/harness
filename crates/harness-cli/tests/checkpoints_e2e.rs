use assert_cmd::Command;
use predicates::str::contains;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn stream(chunks: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(sse(chunks), "text/event-stream")
}

/// A model that writes `hello.txt` and then says `done`.
async fn write_then_answer(server: &MockServer) {
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": "stop"}]})]))
        .with_priority(1)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .respond_with(stream(&[json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "c1", "type": "function",
            "function": {"name": "write", "arguments": "{\"path\":\"hello.txt\",\"content\":\"hi\\n\"}"}}]}, "finish_reason": "tool_calls"}]})]))
        .with_priority(2)
        .mount(server)
        .await;
}

fn env(server_uri: &str) -> (TempDir, TempDir) {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{server_uri}/v1\"\n"),
    )
    .unwrap();
    (home, ws)
}

fn cmd(home: &TempDir, ws: &TempDir) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .env("HARNESS_CREDENTIAL_STORE", "file")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME");
    cmd
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_writes_takes_a_checkpoint() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri());
    // Not a git repository: checkpoints work anyway (ask mode would block the write). The test's
    // data directory is in the temp directory, which sandboxed commands can write to, so there is
    // no sandbox here: checkpoints would be off otherwise.
    let output = tokio::task::spawn_blocking(move || {
        let output = cmd(&home, &ws)
            .env("HARNESS_SANDBOX", "none")
            .args(["--mode", "auto", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).unwrap(),
            "hi\n"
        );
        assert!(home.path().join("data/checkpoints").is_dir());
        output
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let checkpoint = events
        .iter()
        .position(|e| e["type"] == "checkpoint_created")
        .expect("a checkpoint");
    let write = events
        .iter()
        .position(|e| e["type"] == "tool_call_finished")
        .unwrap();
    assert!(checkpoint < write);
}

// Spec: git missing.
#[tokio::test(flavor = "multi_thread")]
async fn without_git_checkpoints_are_disabled_and_turns_proceed() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri());
    tokio::task::spawn_blocking(move || {
        // No sandbox, as above: the temp directory it makes writable holds the data directory.
        cmd(&home, &ws)
            .env("HARNESS_SANDBOX", "none")
            .env("PATH", "/nonexistent")
            .args(["--mode", "auto", "ask", "make hello.txt"])
            .assert()
            .success()
            .stderr(contains(
                "warning: checkpoints are disabled: git was not found on PATH",
            ))
            .stdout(contains("done"));
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).unwrap(),
            "hi\n"
        );
    })
    .await
    .unwrap();
}

/// Skips the test when this host has no OS sandbox (CI's Linux job requires one).
fn host_has_sandbox() -> bool {
    let ok = harness_sandbox::detect(harness_sandbox::SandboxSettings::default()).is_some();
    if !ok {
        if std::env::var("HARNESS_REQUIRE_LINUX_SANDBOX").as_deref() == Ok("1") {
            panic!("HARNESS_REQUIRE_LINUX_SANDBOX=1 but no sandbox was found");
        }
        eprintln!("skipping: no OS sandbox on this host");
    }
    ok
}

fn checkpoint_events(stdout: &[u8]) -> usize {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "checkpoint_created")
        .count()
}

// Review E minor 9 (probe L): a data directory inside the workspace would put the checkpoint
// repository where the agent can change it, and where snapshots would hold it.
#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_repository_inside_the_workspace_disables_checkpoints() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (_home, ws) = env(&server.uri());
    let inner = ws.path().join("home");
    std::fs::create_dir_all(inner.join("config")).unwrap();
    std::fs::write(
        inner.join("config/config.toml"),
        format!("model = \"mock/test-model\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n", server.uri()),
    )
    .unwrap();
    tokio::task::spawn_blocking(move || {
        let output = Command::new(BIN)
            .current_dir(ws.path())
            .env("HARNESS_HOME", &inner)
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .env("HARNESS_SANDBOX", "none")
            .env_remove("XDG_DATA_HOME")
            .args(["--mode", "auto", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("warning: checkpoints are disabled: the checkpoint repository")
                && stderr.contains("is inside the workspace")
                && stderr.contains("run harness in a project directory"),
            "{stderr}"
        );
        assert_eq!(checkpoint_events(&output.stdout), 0);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("hello.txt")).unwrap(),
            "hi\n"
        );
        assert!(!inner.join("data/checkpoints").exists());
    })
    .await
    .unwrap();
}

// Review E minor 9 (probe P): nor may it be where sandboxed commands can write, such as a
// configured writable root.
#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_repository_sandboxed_commands_can_write_disables_checkpoints() {
    if !host_has_sandbox() {
        return;
    }
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri());
    let data = home.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let config = home.path().join("config/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text = format!(
        "[sandbox]\nwritable_roots = [{:?}]\n",
        data.display().to_string()
    ) + &text;
    // Keys after a table header belong to it: the model settings go first.
    let (sandbox, rest) = text.split_at(text.find("model =").unwrap());
    std::fs::write(&config, format!("{rest}{sandbox}")).unwrap();
    tokio::task::spawn_blocking(move || {
        let output = cmd(&home, &ws)
            .args(["--mode", "auto", "ask", "--json", "make hello.txt"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("warning: checkpoints are disabled: the checkpoint repository"),
            "{stderr}"
        );
        assert_eq!(checkpoint_events(&output.stdout), 0);
    })
    .await
    .unwrap();
}

// Review E minor 14: a run prunes the snapshots of sessions whose files are gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_prunes_the_snapshots_of_sessions_that_are_gone() {
    let server = MockServer::start().await;
    write_then_answer(&server).await;
    let (home, ws) = env(&server.uri());
    tokio::task::spawn_blocking(move || {
        let run = || {
            let output = cmd(&home, &ws)
                .env("HARNESS_SANDBOX", "none")
                .args(["--mode", "auto", "ask", "make hello.txt"])
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        };
        run();
        let shadow = std::fs::read_dir(home.path().join("data/checkpoints"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&shadow)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let live = git(&["for-each-ref", "--format=%(refname)", "refs/harness/"]);
        assert_eq!(live.lines().count(), 1, "{live}");
        let tree = git(&["hash-object", "-t", "tree", "-w", "/dev/null"]);
        let old = git(&["commit-tree", tree.trim(), "-m", "a session that is gone"]);
        git(&[
            "update-ref",
            "refs/harness/20200101T000000Z-deadbeef",
            old.trim(),
        ]);
        run();
        let refs = git(&["for-each-ref", "--format=%(refname)", "refs/harness/"]);
        assert!(!refs.contains("deadbeef"), "{refs}");
        assert!(refs.contains(live.trim()), "{refs}");
    })
    .await
    .unwrap();
}
