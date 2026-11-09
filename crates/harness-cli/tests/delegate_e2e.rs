//! `claude-code/*` and `codex/*` through the real binary: `ask` (plain and `--json`), `models`,
//! approvals, sessions and the ledger, with the transport crate's scripted fake child standing in
//! for `claude` and `codex` (and a real Codex recording replayed). No live program is run.
mod common;
use common::Isolate;

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Once,
};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

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
    fn new(config: &str) -> Env {
        // The data directory is outside the system temp directory, which a sandboxed command may
        // write: checkpoints are refused to live in one.
        let home = tempfile::Builder::new()
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        let ws = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(home.path().join("config/config.toml"), config).unwrap();
        std::fs::create_dir(ws.path().join(".git")).unwrap();
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

    /// The environment the program was started with (`<tools>/<name>.env`).
    fn env_of(&self, name: &str) -> String {
        std::fs::read_to_string(self.tools.path().join(format!("{name}.env"))).unwrap_or_default()
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

const CLAUDE_VERSION: &str = "2.1.312 (Claude Code)";
const CODEX_VERSION: &str = "codex-cli 0.154.0";
const HELP: &str = "Options:\n  --model <model>  Provide an alias (e.g. 'sonnet' or 'opus').\n";
const INIT: &str = r#"{"type":"system","subtype":"init","apiKeySource":"none","claude_code_version":"2.1.312","model":"claude-x","session_id":"sess-9"}"#;
const ASSISTANT: &str = r#"{"type":"assistant","message":{"id":"m1","model":"claude-x","role":"assistant","content":[{"type":"text","text":"Hello there"}]},"parent_tool_use_id":null,"session_id":"sess-9"}"#;
const TOOL: &str = r#"{"type":"assistant","message":{"id":"m2","model":"claude-x","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"cargo test"}}]},"parent_tool_use_id":null,"session_id":"sess-9"}"#;
const ASK: &str = r#"{"type":"control_request","request_id":"req_1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cargo test"},"tool_use_id":"toolu_2","decision_reason":"run the tests"}}"#;
const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.25,"result":"ok","usage":{"input_tokens":3,"output_tokens":5},"modelUsage":{},"permission_denials":[],"session_id":"sess-9"}"#;

fn happy() -> String {
    format!("expect user\nemit {INIT}\nemit {ASSISTANT}\nemit {RESULT}\n")
}

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

fn json_events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn a_codex_run_replayed_from_a_real_recording_shows_like_any_turn_in_json() {
    let env = Env::new("");
    let codex = env.fake("codex", CODEX_VERSION, "", &codex_script(env.ws.path()));
    let out = env
        .cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "full-access",
            "--yes-full-access",
            "ask",
            "--json",
            "read",
            "hello.txt",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let events = json_events(&out.stdout);
    let kinds = types(&events);
    assert_eq!(kinds.first().map(String::as_str), Some("turn_started"));
    assert_eq!(kinds.last().map(String::as_str), Some("turn_finished"));
    assert_eq!(events.last().unwrap()["reason"], "completed");
    for needed in [
        "assistant_message",
        "tool_call_requested",
        "tool_call_finished",
        "usage",
        "rate_limits",
        "checkpoint_created",
        "metered",
    ] {
        assert!(kinds.iter().any(|k| k == needed), "no {needed}: {kinds:?}");
    }
    let said: String = events
        .iter()
        .filter(|e| e["type"] == "assistant_message")
        .map(|e| e["content"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(said.contains("PINEAPPLE"), "{said}");
    assert!(
        events
            .iter()
            .filter(|e| e["type"] == "assistant_message")
            .all(|e| e["model"] == "codex/gpt-5.6-luna")
    );
    // The file change is a tool row, the command another.
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "tool_call_requested" && e["name"] == "file_change")
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "tool_call_requested" && e["name"] == "command")
    );
    // The ledger names the run's account; the session keeps the child's thread id.
    let ledger: String = env.kept("jsonl").into_iter().map(|(_, t)| t).collect();
    assert!(ledger.contains("\"account\":\"delegated\""), "{ledger}");
    assert!(ledger.contains("codex/gpt-5.6-luna"), "{ledger}");
    assert!(
        ledger.contains("01a1be41-ffaf-7ce0-95f2-3a9d561469d7"),
        "the thread id is kept in the session"
    );
}

#[test]
fn plain_ask_prints_the_childs_last_answer() {
    let env = Env::new("");
    let codex = env.fake("codex", CODEX_VERSION, "", &codex_script(env.ws.path()));
    env.cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "full-access",
            "--yes-full-access",
            "ask",
            "read",
            "it",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("PINEAPPLE"));
}

#[test]
fn an_approval_nobody_can_answer_blocks_and_exits_3() {
    // Mode `ask`: a file change needs approval, and `ask` has nobody to ask.
    let env = Env::new("");
    let codex = env.fake("codex", CODEX_VERSION, "", &codex_script(env.ws.path()));
    env.cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "ask",
            "ask",
            "go",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("blocked"));
}

#[test]
fn a_claude_command_the_rules_deny_is_refused_without_asking_anyone() {
    let env = Env::new(
        "[delegate.claude]\nuse_subscription = true\n[permissions]\ndeny = [\"bash:cargo test\"]\n",
    );
    let script = format!(
        "expect user\nemit {INIT}\nemit {TOOL}\nemit {ASK}\nexpect req_1\nemit {ASSISTANT}\nemit {RESULT}\n"
    );
    let claude = env.fake("claude", CLAUDE_VERSION, HELP, &script);
    let out = env
        .cmd()
        .env("HARNESS_CLAUDE_BIN", &claude)
        .args([
            "--model",
            "claude-code/sonnet",
            "--mode",
            "auto",
            "ask",
            "--json",
            "test",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let answer = env
        .log("claude")
        .lines()
        .find(|l| l.contains("req_1") && l.starts_with("in:"))
        .map(String::from)
        .expect("the child was answered");
    assert!(answer.contains("deny"), "{answer}");
    let events = json_events(&out.stdout);
    // Decided by the rule: no approval event for it.
    assert!(
        !types(&events).iter().any(|t| t == "approval_needed"),
        "{events:?}"
    );
    // Nothing of the arguments is persisted raw beyond what the child itself said as events.
    let sessions: String = env.kept("jsonl").into_iter().map(|(_, t)| t).collect();
    assert!(!sessions.contains("decision_reason"), "{sessions}");
}

#[test]
fn claude_code_without_an_opt_in_or_a_key_says_how_to_get_one() {
    let env = Env::new("");
    let claude = env.fake("claude", CLAUDE_VERSION, HELP, &happy());
    let out = env
        .cmd()
        .env("HARNESS_CLAUDE_BIN", &claude)
        .args(["--model", "claude-code/sonnet", "ask", "hi"])
        .output()
        .unwrap();
    // Invalid usage, as a hosted model with no key is: decided before any turn.
    assert_eq!(out.status.code(), Some(2), "{:?}", out.status);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("use_subscription") && err.contains("harness auth add anthropic"),
        "{err}"
    );
    assert!(
        env.log("claude").is_empty(),
        "the program was never started"
    );
}

#[test]
fn claude_code_on_a_key_says_so_and_starts_with_the_key_only() {
    let env = Env::new("");
    // With a key, `system/init` says the key is what the program uses.
    let script = happy().replace(
        "\"apiKeySource\":\"none\"",
        "\"apiKeySource\":\"ANTHROPIC_API_KEY\"",
    );
    let claude = env.fake("claude", CLAUDE_VERSION, HELP, &script);
    let out = env
        .cmd()
        .env("HARNESS_CLAUDE_BIN", &claude)
        .env("ANTHROPIC_API_KEY", "sk-ant-api03-test")
        .args(["--model", "claude-code/sonnet", "ask", "--json", "hi"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = json_events(&out.stdout);
    assert!(events.iter().any(|e| {
        e["type"] == "warning"
            && e["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Anthropic API key")
    }));
    // The key is in the child's environment, never in what harness wrote.
    for (_, text) in env.kept("jsonl") {
        assert!(!text.contains("sk-ant-api03-test"), "{text}");
    }
    assert!(!String::from_utf8_lossy(&out.stdout).contains("sk-ant-api03-test"));
    let ledger: String = env.kept("jsonl").into_iter().map(|(_, t)| t).collect();
    assert!(
        ledger.contains("\"account\":\"api_key\""),
        "an API-key run is billed: {ledger}"
    );
}

#[test]
fn with_the_opt_in_the_subscription_is_used_and_a_second_run_resumes_the_session() {
    let env = Env::new("[delegate.claude]\nuse_subscription = true\n");
    let claude = env.fake(
        "claude",
        CLAUDE_VERSION,
        HELP,
        &format!("{}{}", happy(), ""),
    );
    for again in [false, true] {
        // Each run is a process: the script plays again.
        let mut cmd = env.cmd();
        if again {
            cmd.arg("-c");
        }
        cmd.env("HARNESS_CLAUDE_BIN", &claude)
            .env("ANTHROPIC_API_KEY", "sk-ant-api03-test")
            .args(["--model", "claude-code/sonnet", "ask", "hi"])
            .assert()
            .success()
            .stdout(predicates::str::contains("Hello there"));
    }
    let log = env.log("claude");
    let argvs: Vec<&str> = log.lines().filter(|l| l.starts_with("argv:")).collect();
    assert_eq!(argvs.len(), 2, "{log}");
    assert!(!argvs[0].contains("--resume"), "{}", argvs[0]);
    assert!(argvs[1].contains("--resume\u{1f}sess-9"), "{}", argvs[1]);
    // Subscription mode: the key in the environment never reached the child.
    let seen = env.env_of("claude");
    assert!(!seen.is_empty(), "the child's environment was recorded");
    assert!(!seen.contains("ANTHROPIC_API_KEY"), "{seen}");
    assert!(!seen.contains("sk-ant-api03-test"), "{seen}");
}

#[test]
fn full_access_needs_the_flag_when_nobody_can_confirm() {
    let env = Env::new("");
    let codex = env.fake("codex", CODEX_VERSION, "", &codex_script(env.ws.path()));
    let refused = env
        .cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "full-access",
            "ask",
            "go",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes-full-access"));
    assert!(env.log("codex").is_empty(), "nothing ran");
    env.cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "full-access",
            "--yes-full-access",
            "ask",
            "go",
        ])
        .assert()
        .success();
}

#[test]
fn an_untested_version_is_refused_with_the_range_unless_allowed() {
    let env = Env::new("");
    let codex = env.fake(
        "codex",
        "codex-cli 0.200.0",
        "",
        &codex_script(env.ws.path()),
    );
    env.cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args(["--model", "codex/gpt-5.6-luna", "ask", "go"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("outside the tested range"));
}

#[test]
fn models_lists_what_can_be_used_and_says_why_the_rest_cannot() {
    let env = Env::new("");
    let claude = env.fake("claude", CLAUDE_VERSION, HELP, "");
    let out = env
        .cmd()
        .env("HARNESS_CLAUDE_BIN", &claude)
        .env("HARNESS_CODEX_BIN", "/nonexistent/codex")
        .arg("models")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("claude-code/sonnet") && stdout.contains("claude-code/opus"),
        "{stdout}"
    );
    assert!(!stdout.contains("codex/"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("codex: not listed"), "{stderr}");
}

#[test]
fn models_says_nothing_about_a_program_that_is_not_installed() {
    let env = Env::new("");
    let out = env
        .cmd()
        .env("PATH", "/nonexistent")
        .env_remove("HARNESS_CLAUDE_BIN")
        .env_remove("HARNESS_CODEX_BIN")
        .arg("models")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("not listed"), "{stderr}");
}

#[test]
fn the_flag_that_confirms_full_access_is_refused_where_only_ask_can_use_it() {
    let env = Env::new("");
    let out = env
        .cmd()
        .args(["--yes-full-access", "models"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("only works with `harness ask`"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_untested_version_is_run_when_allow_untested_is_set() {
    let env = Env::new("[delegate]\nallow_untested = true\n");
    let codex = env.fake(
        "codex",
        "codex-cli 0.200.0",
        "",
        &codex_script(env.ws.path()),
    );
    env.cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "full-access",
            "--yes-full-access",
            "ask",
            "go",
        ])
        .assert()
        .success();
}

#[test]
fn models_lists_what_codex_answers_to_model_list() {
    let env = Env::new("");
    let script = "expect \"method\":\"initialize\"\n\
        emit {\"id\":1,\"result\":{\"userAgent\":\"codex/0.154.0\"}}\n\
        expect \"method\":\"initialized\"\n\
        expect \"method\":\"model/list\"\n\
        emit {\"id\":2,\"result\":{\"data\":[{\"id\":\"a\",\"model\":\"gpt-5-codex\"},{\"id\":\"b\",\"model\":\"gpt-5.1\",\"hidden\":true}],\"nextCursor\":null}}\n\
        until-eof\n";
    let codex = env.fake("codex", CODEX_VERSION, "", script);
    let out = env
        .cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .env("HARNESS_CLAUDE_BIN", "/nonexistent/claude")
        .arg("models")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("codex/gpt-5-codex"), "{stdout}");
    assert!(!stdout.contains("gpt-5.1"), "a hidden model: {stdout}");
}

#[test]
fn in_auto_mode_a_command_nobody_allowed_is_blocked_because_the_program_has_no_sandbox() {
    let env = Env::new("");
    let codex = env.fake("codex", CODEX_VERSION, "", &codex_script(env.ws.path()));
    env.cmd()
        .env("HARNESS_CODEX_BIN", &codex)
        .args([
            "--model",
            "codex/gpt-5.6-luna",
            "--mode",
            "auto",
            "ask",
            "go",
        ])
        .assert()
        .code(3)
        .stderr(predicates::str::contains("cannot sandbox"));
}
