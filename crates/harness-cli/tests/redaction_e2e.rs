//! The canary tests for task 4.7: a run whose API key and secret-looking environment variables
//! reach the conversation (through `printenv`, a model that repeats them in pieces, output long
//! enough to be cut through them, and a tool call that quotes a password) leaves none of them in
//! the session file, the tool-output files, the debug log, the NDJSON output or what harness
//! prints.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use common::Isolate;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");
const KEY: &str = "sk-canary-key-qzvwjxpkfm";
const TOKEN: &str = "canary-tqkzn-hgtrdwsmlq";
/// A password with a quote and a backslash, which JSON escapes.
const PASSWORD: &str = r#"pa"ss\wd-canary-bnfhq"#;

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

fn text(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"content": text}}]})
}

fn reasoning(text: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"reasoning_content": text}}]})
}

fn stop() -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
}

/// A reply that calls each of `calls`, `(id, tool, arguments)`.
fn tool_calls(calls: &[(&str, &str, Value)]) -> Value {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({"index": index, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}})
        })
        .collect();
    json!({"choices": [{"index": 0, "delta": {"tool_calls": calls}, "finish_reason": "tool_calls"}]})
}

/// Answers the requests in turn with `replies`, the last one again after that.
struct Replies {
    replies: Vec<ResponseTemplate>,
    next: AtomicUsize,
}

impl Respond for Replies {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let next = self.next.fetch_add(1, Ordering::SeqCst);
        self.replies[next.min(self.replies.len() - 1)].clone()
    }
}

/// Every file under `dir`, as text. Every file harness writes is text, so one that is not fails.
fn files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is not text: {e}", path.display()));
            found.push((path, text));
        }
    }
    found
}

/// The five-character pieces of `secret`: a piece of it that a cut or a stream lets through
/// holds one of them.
fn pieces(secret: &str) -> Vec<&str> {
    (0..=secret.len() - 5).map(|i| &secret[i..i + 5]).collect()
}

/// What a run printed and wrote.
struct Run {
    home: TempDir,
    stdout: String,
    stderr: String,
    code: Option<i32>,
    requests: Vec<Request>,
}

/// Where a [`Run`] runs, besides its defaults.
#[derive(Default)]
struct Scenario<'a> {
    /// More environment variables.
    vars: &'a [(&'a str, &'a str)],
    /// Files in the workspace, `(path, content)`.
    workspace: &'a [(&'a str, &'a str)],
    /// Files in `HARNESS_HOME`, `(path, content)`.
    home: &'a [(&'a str, &'a str)],
    /// More of the global configuration.
    config: &'a str,
}

impl Run {
    /// Runs `harness <args>` against a mock provider that answers with `replies`, with `KEY` as
    /// its API key and `vars` in the environment.
    async fn new(replies: Vec<ResponseTemplate>, vars: &[(&str, &str)], args: &[&str]) -> Run {
        Run::with(
            replies,
            args,
            Scenario {
                vars,
                ..Scenario::default()
            },
        )
        .await
    }

    /// [`Run::new`], in `scenario`.
    async fn with(replies: Vec<ResponseTemplate>, args: &[&str], scenario: Scenario<'_>) -> Run {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(Replies {
                replies,
                next: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let home = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "model = \"mock/m\"\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\napi_key_env = \"MOCK_API_KEY\"\n[profiles.\"mock/*\"]\ncontext_window = 32768\n{}",
                server.uri(),
                scenario.config
            ),
        )
        .unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
        for (dir, files) in [
            (ws.path(), scenario.workspace),
            (home.path(), scenario.home),
        ] {
            for (path, content) in files {
                let path = dir.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
        }
        let vars: Vec<(String, String)> = scenario
            .vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let (home, output) = tokio::task::spawn_blocking(move || {
            use std::{ffi::OsString, os::unix::ffi::OsStringExt};
            let output = Command::new(BIN)
                .current_dir(ws.path())
                .env("HARNESS_HOME", home.path())
                .isolate()
                .env("MOCK_API_KEY", KEY)
                // Review F I1: a variable that is not UTF-8 is no reason to stop.
                .env("LEGACY_NAME", OsString::from_vec(b"caf\xe9".to_vec()))
                .envs(vars)
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_DATA_HOME")
                .env_remove("XDG_STATE_HOME")
                .args(args)
                .output()
                .unwrap();
            drop(ws);
            (home, output)
        })
        .await
        .unwrap();
        Run {
            home,
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
            code: output.status.code(),
            requests: server.received_requests().await.unwrap(),
        }
    }

    /// Everything the run printed and wrote: stdout, stderr and each file under its home.
    fn written(&self) -> Vec<(String, String)> {
        let mut written = vec![
            ("stdout".to_string(), self.stdout.clone()),
            ("stderr".to_string(), self.stderr.clone()),
        ];
        for (path, text) in files(self.home.path()) {
            written.push((path.display().to_string(), text));
        }
        written
    }

    /// Checks that a file of each of `kinds` was written.
    fn wrote(&self, kinds: &[&str]) {
        let written = self.written();
        for kind in kinds {
            assert!(
                written.iter().any(|(name, _)| name.contains(kind)),
                "no {kind} file: {:?}",
                written.iter().map(|(n, _)| n).collect::<Vec<_>>()
            );
        }
    }
}

/// `printenv` prints the token; the key, printed back to back, runs through both ends of the
/// cut the tool output gets (4,096 bytes from each end), with `printenv` and a long `seq` between
/// them, which only the tool-output file holds.
const COMMAND: &str = r#"printenv DEPLOY_TOKEN; yes "$MOCK_API_KEY" | head -n 250 | tr -d '\n'; echo; printenv; seq 1 4000; yes "$MOCK_API_KEY" | head -n 250 | tr -d '\n'"#;

/// The model runs [`COMMAND`], then streams the key and the token back in pieces, in its
/// reasoning and in its answer.
fn printenv_replies() -> Vec<ResponseTemplate> {
    let (key_head, key_tail) = KEY.split_at(10);
    let (token_head, token_tail) = TOKEN.split_at(11);
    vec![
        sse(&[tool_calls(&[("c1", "bash", json!({"command": COMMAND}))])]),
        sse(&[
            reasoning("They asked for the key, "),
            reasoning(key_head),
            reasoning(key_tail),
            reasoning("."),
            text("Your key is "),
            text(key_head),
            text(&key_tail[..4]),
            text(&key_tail[4..]),
            text(" and the token is "),
            text(token_head),
            text(&format!("{token_tail}.")),
            stop(),
        ]),
    ]
}

/// Checks that nothing `run` printed or wrote holds a piece of the key or the token.
fn holds_no_piece_of_a_secret(run: &Run) {
    for (what, text) in run.written() {
        for piece in pieces(KEY).into_iter().chain(pieces(TOKEN)) {
            assert!(!text.contains(piece), "{what} holds {piece}:\n{text}");
        }
    }
}

// Spec: "Debug logging" and "A command prints the environment". Review F C1 and I2: the model's
// reply streamed in pieces, and tool output cut through a secret.
#[tokio::test(flavor = "multi_thread")]
async fn no_secret_is_written_anywhere() {
    let run = Run::new(
        printenv_replies(),
        &[("DEPLOY_TOKEN", TOKEN)],
        &["--debug", "ask", "--json", "show the env"],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(run.stderr.contains("debug log: "), "{}", run.stderr);
    run.wrote(&["/data/sessions/", "/state/tool-output/", "/state/logs/"]);
    holds_no_piece_of_a_secret(&run);
    // The streamed answer is whole, and redacted.
    let events: Vec<Value> = run
        .stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let streamed = |kind: &str| -> String {
        events
            .iter()
            .filter(|e| e["type"] == kind)
            .map(|e| e["text"].as_str().unwrap())
            .collect()
    };
    assert_eq!(
        streamed("text_delta"),
        "Your key is [redacted] and the token is [redacted]."
    );
    assert_eq!(
        streamed("reasoning_delta"),
        "They asked for the key, [redacted]."
    );
    // The cut left each copy of the key whole, on one side or the other.
    let output = events
        .iter()
        .find(|e| e["type"] == "tool_call_finished")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let (head, rest) = output.split_once("\n[... ").unwrap();
    let (_, tail) = rest.split_once(" ...]\n").unwrap();
    assert!(head.ends_with("[redacted]"), "{head}");
    assert!(tail.starts_with("[redacted]"), "{tail}");
    // The model still saw the output as it was.
    let seen = String::from_utf8_lossy(&run.requests[1].body);
    assert!(seen.contains(TOKEN) && seen.contains(KEY));
}

// Review F M4: in plain mode the answer goes to stdout and the tool call to stderr.
#[tokio::test(flavor = "multi_thread")]
async fn no_secret_is_printed_in_plain_mode() {
    let run = Run::new(
        printenv_replies(),
        &[("DEPLOY_TOKEN", TOKEN)],
        &["--debug", "ask", "show the env"],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(
        run.stdout,
        "Your key is [redacted] and the token is [redacted].\n"
    );
    assert!(run.stderr.contains("-> bash "), "{}", run.stderr);
    run.wrote(&["/data/sessions/", "/state/tool-output/", "/state/logs/"]);
    holds_no_piece_of_a_secret(&run);
}

/// The model runs a command that quotes [`PASSWORD`], then tries to write it outside the
/// workspace, which is blocked (and shown on stderr in plain mode), then says it is done.
fn password_replies() -> Vec<ResponseTemplate> {
    let content = format!("password = '{PASSWORD}'\n");
    vec![
        sse(&[tool_calls(&[
            (
                "c1",
                "bash",
                json!({"command": format!("echo 'the password is {PASSWORD}' >/dev/null; echo done")}),
            ),
            (
                "c2",
                "write",
                json!({"path": "../outside.conf", "content": content}),
            ),
        ])]),
        sse(&[text("I could not write it."), stop()]),
    ]
}

// Review F I3 (probe P4): a password with a quote and a backslash inside a tool call's arguments
// is escaped twice in the event and the session entry. It must not survive anywhere, stderr
// included.
#[tokio::test(flavor = "multi_thread")]
async fn a_password_in_a_tool_call_is_written_nowhere() {
    for json in [false, true] {
        let mut args = vec!["--debug", "ask"];
        if json {
            args.push("--json");
        }
        args.push("set up the database");
        let run = Run::new(password_replies(), &[("DB_PASSWORD", PASSWORD)], &args).await;
        // The write was blocked for lack of approval.
        assert_eq!(run.code, Some(3), "{}", run.stderr);
        run.wrote(&["/data/sessions/", "/state/logs/"]);
        if json {
            assert!(run.stdout.contains("tool_call_requested"), "{}", run.stdout);
        } else {
            assert!(run.stderr.contains("-> bash "), "{}", run.stderr);
            assert!(run.stderr.contains("proposed content of"), "{}", run.stderr);
        }
        for (what, text) in run.written() {
            assert!(!text.contains("wd-canary-bnfhq"), "{what}:\n{text}");
        }
        // The model's request carried the password as the model wrote it.
        let seen = String::from_utf8_lossy(&run.requests[1].body);
        assert!(seen.contains("wd-canary-bnfhq"));
    }
}

// Review F M1: every key and sign-in token harness holds is a secret from the start, not only
// the key this run uses: a command that prints the credential file, and the key variable of a
// configured provider whose name no rule marks, writes none of them.
#[tokio::test(flavor = "multi_thread")]
async fn stored_credentials_the_run_does_not_use_are_secrets_too() {
    const STORED: &str = "sk-stored-openai-mvbqzrtkwx";
    const ACCESS: &str = "access-jwt-hdkqzmvbtr";
    const REFRESH: &str = "refresh-rt-pxwqlzmnvc";
    const OTHER: &str = "other-cred-kzqwmvbxtr";
    let signed_in =
        json!({"access_token": ACCESS, "refresh_token": REFRESH, "account_id": "acct-1"});
    let credentials = json!({"credentials": {
        "openai/default": STORED,
        "chatgpt/work": signed_in.to_string(),
    }})
    .to_string();
    let command = r#"cat "$HARNESS_HOME/data/credentials.json"; printenv OTHER_CRED"#;
    let run = Run::with(
        vec![
            sse(&[tool_calls(&[("c1", "bash", json!({"command": command}))])]),
            sse(&[
                text(&format!("It holds {STORED}, {ACCESS} and {OTHER}.")),
                stop(),
            ]),
        ],
        &["--debug", "ask", "--json", "show the credentials"],
        Scenario {
            vars: &[("OTHER_CRED", OTHER)],
            home: &[("data/credentials.json", &credentials)],
            config: "[providers.other]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key_env = \"OTHER_CRED\"\n",
            ..Scenario::default()
        },
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    // The model saw them: they were printed.
    let seen = String::from_utf8_lossy(&run.requests[1].body);
    assert!(seen.contains(STORED) && seen.contains(ACCESS) && seen.contains(OTHER));
    for (what, text) in run.written() {
        if what.ends_with("/data/credentials.json") {
            continue;
        }
        for secret in [STORED, ACCESS, REFRESH, OTHER] {
            assert!(!text.contains(secret), "{what} holds {secret}:\n{text}");
        }
    }
}

// Review F M8: the warnings printed before the agent starts (here from the configuration, from
// `ask` itself and from reading the instruction files) are in the debug log too, redacted.
#[tokio::test(flavor = "multi_thread")]
async fn the_debug_log_holds_the_warnings_printed_at_startup() {
    let run = Run::with(
        vec![sse(&[text("hi"), stop()])],
        &["--debug", "--mode", "full-access", "ask", "hi"],
        Scenario {
            workspace: &[
                (
                    ".harness/config.toml",
                    "[permissions]\nallow = [\"bash:make*\"]\n",
                ),
                ("AGENTS.md", &format!("Be brief.\n@{KEY}.md\n")),
            ],
            ..Scenario::default()
        },
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let log = run
        .written()
        .into_iter()
        .find(|(name, _)| name.contains("/state/logs/"))
        .unwrap()
        .1;
    for warning in [
        "ignoring 1 setting(s) that widen what the agent may do",
        "full-access mode: commands run without approval or sandbox",
        "skipped import @[redacted].md in ",
    ] {
        assert!(run.stderr.contains(warning), "{warning}: {}", run.stderr);
        assert!(
            log.lines().any(|line| line.contains(warning)
                && serde_json::from_str::<Value>(line).unwrap()["type"] == "warning"),
            "{warning}: {log}"
        );
    }
    holds_no_piece_of_a_secret(&run);
}

// Review F M6: `--debug` logs a run of `harness ask`; with another command it did nothing,
// silently.
#[test]
fn debug_with_another_command_is_refused() {
    let home = TempDir::new().unwrap();
    for (args, command) in [
        (&["--debug", "models"][..], "harness models"),
        (&["trust", "--debug", "--yes"], "harness trust"),
        (&["sandbox", "doctor", "--debug"], "harness sandbox doctor"),
        (
            &["auth", "use", "openai", "work", "--debug"],
            "harness auth use",
        ),
        (&["logout", "openai", "--debug"], "harness logout"),
    ] {
        let output = Command::new(BIN)
            .args(args)
            .env("HARNESS_HOME", home.path())
            .env("HARNESS_CREDENTIAL_STORE", "file")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "--debug logs a run of `harness ask`; run `{command}` without it"
            )),
            "{args:?}: {stderr}"
        );
    }
}

// Review F I1: one environment variable that is not UTF-8, in its name or its value, made every
// command panic at startup (exit 101).
#[test]
fn a_variable_that_is_not_utf8_does_not_stop_harness() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let output = Command::new(BIN)
        .current_dir(ws.path())
        .env("HARNESS_HOME", home.path())
        .isolate()
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("LEGACY_NAME", OsString::from_vec(b"caf\xe9".to_vec()))
        .env(
            OsString::from_vec(b"CAF\xc9_TOKEN".to_vec()),
            "legacy-token-value",
        )
        .arg("models")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
