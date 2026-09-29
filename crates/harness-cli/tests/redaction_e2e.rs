//! The canary test for task 4.7: a run whose API key and a secret-looking environment variable
//! reach the conversation through `printenv` leaves neither in the session file, the tool-output
//! files, the debug log, the NDJSON output or what harness prints.

mod common;
use common::Isolate;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");
const KEY: &str = "sk-canary-key-0123456789";
const TOKEN: &str = "canary-token-9876543210";

fn stream(chunk: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        "text/event-stream",
    )
}

/// Every file under `dir`, as text.
fn files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            found.push((path, text));
        }
    }
    found
}

// Spec: "Debug logging" and "A command prints the environment".
#[tokio::test(flavor = "multi_thread")]
async fn no_secret_is_written_anywhere() {
    let server = MockServer::start().await;
    // Second request: the tool ran. The model repeats the key it saw.
    Mock::given(method("POST"))
        .and(body_string_contains("\"role\":\"tool\""))
        .respond_with(stream(json!({"choices": [{"index": 0,
            "delta": {"content": format!("Your key is {KEY}.")}, "finish_reason": "stop"}]})))
        .with_priority(1)
        .mount(&server)
        .await;
    // The secrets first, so that the model's share of the output holds them; then enough output
    // that it is saved to a tool-output file.
    let command = "printenv DEPLOY_TOKEN MOCK_API_KEY; printenv; seq 1 4000";
    Mock::given(method("POST"))
        .respond_with(stream(json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
            "id": "c1", "type": "function", "function": {"name": "bash",
            "arguments": json!({"command": command}).to_string()}}]}, "finish_reason": "tool_calls"}]})))
        .with_priority(2)
        .mount(&server)
        .await;
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/config.toml"),
        format!(
            "model = \"mock/m\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key_env = \"MOCK_API_KEY\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n",
            server.uri()
        ),
    )
    .unwrap();
    std::fs::create_dir(ws.path().join(".git")).unwrap();
    let (home, ws, output) = tokio::task::spawn_blocking(move || {
        let output = Command::new(BIN)
            .current_dir(ws.path())
            .env("HARNESS_HOME", home.path())
            .isolate()
            .env("MOCK_API_KEY", KEY)
            .env("DEPLOY_TOKEN", TOKEN)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .args(["--debug", "ask", "--json", "show the env"])
            .output()
            .unwrap();
        (home, ws, output)
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    for (what, text) in [("stdout", stdout.as_ref()), ("stderr", stderr.as_ref())] {
        assert!(
            !text.contains(KEY) && !text.contains(TOKEN),
            "{what}: {text}"
        );
    }
    assert!(stdout.contains("[redacted]"), "{stdout}");
    assert!(stderr.contains("debug log: "), "{stderr}");
    let written = files(home.path());
    let names: Vec<String> = written
        .iter()
        .map(|(p, _)| p.display().to_string())
        .collect();
    for kind in ["/data/sessions/", "/state/tool-output/", "/state/logs/"] {
        assert!(
            names.iter().any(|n| n.contains(kind)),
            "no {kind} file: {names:?}"
        );
    }
    for (path, text) in &written {
        assert!(
            !text.contains(KEY) && !text.contains(TOKEN),
            "{} holds a secret",
            path.display()
        );
    }
    // The model still saw the output as it was.
    let requests = server.received_requests().await.unwrap();
    assert!(String::from_utf8_lossy(&requests[1].body).contains(TOKEN));
    drop(ws);
}
