//! M3d part 2: the main agent sends sub-tasks to a Claude Code or Codex program as subagents,
//! through the real binary. The main model and one subagent are mock HTTP servers; the Codex
//! program is the transport crate's scripted fake replaying a real recording. No live program.
mod common;
use common::Isolate;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Once,
    time::Duration,
};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

/// The fake child binary: next to `harness` in the target directory; built once if it is not.
fn fake_child() -> PathBuf {
    static BUILD: Once = Once::new();
    let path = Path::new(BIN).parent().unwrap().join("fake-delegate-child");
    BUILD.call_once(|| {
        if !path.exists() {
            let status = std::process::Command::new(env!("CARGO"))
                .args([
                    "build",
                    "-p",
                    "harness-delegate",
                    "--bin",
                    "fake-delegate-child",
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }
    });
    path
}

struct Env {
    home: TempDir,
    ws: TempDir,
    tools: TempDir,
}

impl Env {
    /// A project in a git repository with one commit, the user's agent definitions in
    /// `agents` (name, text), and a mock model server at `uri` as the session's model.
    fn new(uri: &str, config: &str, agents: &[(&str, &str)]) -> Env {
        // The data directory is outside the system temp directory, which a sandboxed command may
        // write: checkpoints are refused to live in one.
        let home = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!("model = \"mock/test-model\"\n{config}\n[providers.mock]\nprotocol = \"openai-chat\"\nbase_url = \"{uri}/v1\"\n"),
        )
        .unwrap();
        let defs = home.path().join("config/agents");
        std::fs::create_dir_all(&defs).unwrap();
        for (name, text) in agents {
            std::fs::write(defs.join(format!("{name}.md")), text).unwrap();
        }
        harness_core::worktree::testing::init_repo(ws.path());
        Env {
            home,
            ws,
            tools: tempfile::tempdir().unwrap(),
        }
    }

    /// A fake `name` ("claude" or "codex") answering `--version` with `version`, `mcp list` with
    /// no servers, `--help` with `help`, and otherwise playing `script`. Its log is
    /// `<tools>/<name>.log`.
    fn fake(&self, name: &str, version: &str, help: &str, script: &str) -> PathBuf {
        let script_path = self.tools.path().join(format!("{name}.script"));
        std::fs::write(&script_path, script).unwrap();
        let help_path = self.tools.path().join(format!("{name}.help"));
        std::fs::write(&help_path, help).unwrap();
        let log = self.tools.path().join(format!("{name}.log"));
        let binary = self.tools.path().join(name);
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\n\
                 if [ \"$1\" = \"--help\" ]; then cat '{}'; exit 0; fi\n\
                 if [ \"$1\" = \"mcp\" ]; then echo '[]'; exit 0; fi\n\
                 FAKE_CHILD_SCRIPT='{}' FAKE_CHILD_LOG='{}' FAKE_CHILD_ENVDUMP='{}' exec '{}' \"$@\"\n",
                help_path.display(),
                script_path.display(),
                log.display(),
                self.tools.path().join(format!("{name}.env")).display(),
                fake_child().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        binary
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.tools.path().join(format!("{name}.log"))).unwrap_or_default()
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.current_dir(self.ws.path())
            .env("HARNESS_HOME", self.home.path())
            .isolate()
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_STATE_HOME")
            .timeout(std::time::Duration::from_secs(60));
        cmd
    }

    /// Every file under the home with `needle` in its name or text, for checking what was kept.
    fn kept(&self, extension: &str) -> Vec<(PathBuf, String)> {
        fn walk(dir: &Path, extension: &str, out: &mut Vec<(PathBuf, String)>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, extension, out);
                } else if path.extension().is_some_and(|e| e == extension)
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    out.push((path, text));
                }
            }
        }
        let mut out = Vec::new();
        walk(self.home.path(), extension, &mut out);
        out
    }
}

const CODEX_VERSION: &str = "codex-cli 0.154.0";

/// The first turn of the recorded Codex run as a script: what the recording's client wrote is
/// read (not compared: that is the transport's own test), what the server said is emitted.
fn codex_script(workspace: &Path) -> String {
    let workspace = workspace.canonicalize().unwrap().display().to_string();
    let text = include_str!("../../harness-delegate/tests/fixtures/codex/ask-0.154.0.jsonl");
    let mut script = String::new();
    for (i, line) in text.lines().enumerate() {
        if i > 167 {
            break;
        }
        let outer: Value = serde_json::from_str(line).unwrap();
        if outer["dir"] == "in" {
            script.push_str("read\n");
        } else {
            // The recording was made in /work/ws.
            script.push_str(&format!(
                "emit {}\n",
                outer["line"].to_string().replace("/work/ws", &workspace)
            ));
        }
    }
    script
}

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

const ALPHA: &str = "---\ndescription: Looks at the first thing\ntools: Read, Grep\n---\nALPHA-BODY-MARKER: look at the first thing.\n";
const SCOUT: &str = "---\ndescription: Looks, on Codex\nmodel: codex/gpt-5.6-luna\ntools: Read, Grep\n---\nSCOUT-BODY-MARKER: look around.\n";

/// The parent asks for `alpha` (in process, slow) and `scout` (on Codex); once it has their
/// reports it answers.
async fn script(server: &MockServer, agents: Value) {
    let url = || path("/v1/chat/completions");
    Mock::given(method("POST"))
        .and(url())
        .and(body_string_contains("ALPHA-BODY-MARKER"))
        .respond_with(
            stream(&[text_chunk("alpha found three files")]).set_delay(Duration::from_millis(600)),
        )
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
    Mock::given(method("POST"))
        .and(url())
        .respond_with(stream(&[tool_chunk(
            "c1",
            "spawn_agents",
            &json!({ "agents": agents }).to_string(),
        )]))
        .with_priority(3)
        .mount(server)
        .await;
}

fn events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_in_process_subagent_and_a_codex_subagent_run_together_and_both_report() {
    let server = MockServer::start().await;
    script(
        &server,
        json!([
            {"agent": "alpha", "task": "TASK-ONE: inspect the first thing"},
            {"agent": "scout", "task": "TASK-TWO: read hello.txt"},
        ]),
    )
    .await;
    let env = Env::new(
        &server.uri(),
        "[agents]\nper_local = 2\n",
        &[("alpha", ALPHA), ("scout", SCOUT)],
    );
    // The program stays busy for a while, so the two overlap.
    let script = format!("sleep 500\n{}", codex_script(env.ws.path()));
    let codex = env.fake("codex", CODEX_VERSION, "", &script);
    let output = tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HARNESS_CODEX_BIN", &codex)
            .args(["--mode", "auto", "ask", "--json", "go"])
            .output()
            .unwrap();
        (out, env)
    })
    .await
    .unwrap();
    let (out, env) = output;
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let events = events(&out.stdout);
    let kind = |e: &Value, t: &str| e["type"] == t;

    // Both started, the backend named by the model each runs on; both before either finished.
    let started: Vec<&Value> = events.iter().filter(|e| kind(e, "agent_started")).collect();
    assert_eq!(started.len(), 2, "{events:#?}");
    let model_of = |label: &str| {
        started
            .iter()
            .find(|e| e["label"] == label)
            .unwrap_or_else(|| panic!("no {label}"))["model"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(model_of("alpha"), "mock/test-model");
    assert_eq!(model_of("scout"), "codex/gpt-5.6-luna");
    let last_start = events
        .iter()
        .rposition(|e| kind(e, "agent_started"))
        .unwrap();
    let first_finish = events
        .iter()
        .position(|e| kind(e, "agent_finished"))
        .unwrap();
    assert!(
        last_start < first_finish,
        "the children ran one after another"
    );

    // Both reports reached the parent as the tool's result.
    let result = events
        .iter()
        .find(|e| kind(e, "tool_call_finished") && e.get("agent").is_none())
        .expect("the parent's tool call finished");
    assert_eq!(result["is_error"], false, "{result}");
    let report = result["output"].as_str().unwrap();
    assert_eq!(report.matches("status=\"done\"").count(), 2, "{report}");
    assert!(report.contains("alpha found three files"), "{report}");
    assert!(report.contains("model: codex/gpt-5.6-luna"), "{report}");
    assert!(report.contains("PINEAPPLE"), "{report}");
    // What the Codex child did is in its report, from its own tool events.
    assert!(
        !report
            .contains("agent=\"scout\" status=\"done\">\nmodel: codex/gpt-5.6-luna\ntool calls: 0"),
        "{report}"
    );
    // A read-only Codex child needs no worktree and left no branch.
    assert!(!report.contains("branch: harness/scout"), "{report}");

    // The program was started in the project (read-only), and got the definition's text and the
    // task, never the parent's conversation.
    let asked = env.log("codex");
    assert!(asked.contains("SCOUT-BODY-MARKER"), "{asked}");
    assert!(asked.contains("TASK-TWO"), "{asked}");
    assert!(!asked.contains("TASK-ONE"), "{asked}");

    // Its events carry its id, and the ledger names the agent, its parent and the account kind.
    let scout_id = started.iter().find(|e| e["label"] == "scout").unwrap()["agent"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        events
            .iter()
            .any(|e| e["agent"] == scout_id.as_str() && kind(e, "tool_call_requested")),
        "no tool call of {scout_id}"
    );
    let ledger: String = env.kept("jsonl").into_iter().map(|(_, t)| t).collect();
    let line = ledger
        .lines()
        .find(|l| l.contains("codex/gpt-5.6-luna") && l.contains("\"account\":\"delegated\""))
        .unwrap_or_else(|| panic!("no delegated record in {ledger}"));
    assert!(
        line.contains(&format!("\"agent\":\"{scout_id}\"")),
        "{line}"
    );
    assert!(line.contains("\"parent\""), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_subagent_that_cannot_be_used_refuses_the_call_with_the_reason() {
    let server = MockServer::start().await;
    script(
        &server,
        json!([{"agent": "scout", "task": "TASK-TWO: read hello.txt"}]),
    )
    .await;
    let env = Env::new(&server.uri(), "", &[("scout", SCOUT)]);
    // A codex outside the tested range.
    let codex = env.fake("codex", "codex-cli 0.1.0", "", "");
    let out = tokio::task::spawn_blocking(move || {
        env.cmd()
            .env("HARNESS_CODEX_BIN", &codex)
            .args(["--mode", "auto", "ask", "--json", "go"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let events = events(&out.stdout);
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_call_finished" && e.get("agent").is_none())
        .expect("the parent's tool call finished");
    assert_eq!(result["is_error"], true, "{result}");
    let said = result["output"].as_str().unwrap();
    assert!(said.contains("tested range"), "{said}");
    assert!(said.contains("Nothing was started"), "{said}");
    assert!(!events.iter().any(|e| e["type"] == "agent_started"));
}

const WRITER: &str = "---\ndescription: Edits, on Codex\nmodel: codex/gpt-5.6-luna\n---\nWRITER-BODY-MARKER: make the change.\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_subagent_that_can_edit_runs_in_a_worktree_and_the_run_confirms_full_access_for_it()
{
    let server = MockServer::start().await;
    script(
        &server,
        json!([{"agent": "writer", "task": "TASK-TWO: change hello.txt"}]),
    )
    .await;
    let env = Env::new(&server.uri(), "", &[("writer", WRITER)]);
    let codex = env.fake("codex", CODEX_VERSION, "", &codex_script(env.ws.path()));
    let ws = env.ws.path().to_path_buf();
    let out = tokio::task::spawn_blocking(move || {
        let out = env
            .cmd()
            .env("HARNESS_CODEX_BIN", &codex)
            .args([
                "--mode",
                "full-access",
                "--yes-full-access",
                "ask",
                "--json",
                "go",
            ])
            .output()
            .unwrap();
        (out, env)
    })
    .await
    .unwrap();
    let (out, env) = out;
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let events = events(&out.stdout);
    let result = events
        .iter()
        .find(|e| e["type"] == "tool_call_finished" && e.get("agent").is_none())
        .unwrap();
    let report = result["output"].as_str().unwrap();
    assert!(report.contains("status=\"done\""), "{report}");
    // The program started in a worktree harness made, never in the checkout.
    let asked = env.log("codex");
    let cwd = asked
        .lines()
        .find(|l| l.contains("thread/start"))
        .unwrap_or_else(|| panic!("no thread/start in {asked}"));
    assert!(cwd.contains("worktrees"), "{cwd}");
    assert!(
        !cwd.contains(&format!(
            "\"cwd\":\"{}\"",
            ws.canonicalize().unwrap().display()
        )),
        "{cwd}"
    );
    // What the program did is in the report, from its own tool events: its calls and its
    // shell commands (the recording's file change was declined, so no file changed, and no
    // branch was kept), and what it used.
    assert!(report.contains("tool calls: 4"), "{report}");
    assert!(
        report.contains("commands run:") && report.contains("cat hello.txt"),
        "{report}"
    );
    assert!(report.contains("worktree: it changed no file"), "{report}");
    assert!(report.contains("used: "), "{report}");
    // Nothing reached the checkout.
    assert!(
        harness_core::worktree::testing::git(&ws, &["status", "--porcelain"]).is_empty(),
        "the checkout was touched"
    );
}
