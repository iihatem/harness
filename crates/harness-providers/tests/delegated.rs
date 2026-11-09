//! `claude-code/*` and `codex/*` as providers: what a child's events become, and how its
//! approvals reach harness's gate. The child is the transport crate's scripted fake.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Once},
    time::Duration,
};

use async_trait::async_trait;
use harness_core::{
    agent::ApprovalKind,
    delegated::{ApprovalGate, DelegatedOutcome, DelegatedTurn, GateAnswer, GateRequest},
    event::{AgentEvent, TurnEndReason},
    meter::AccountKind,
    permission::{Action, Mode},
    provider::Provider,
    role::Role,
};
use harness_delegate::{
    BackendKind, ClaudeBackend,
    env::ApiKey,
    event::{DelegateApproval, DelegateApprovalKind},
};
use harness_providers::delegated::{
    DelegateSettings, DelegatedProvider, OptInStore,
    approve::{self, PLAN_NOTE, Step},
    optin::{self, Auth},
};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The transport crate's fake child, next to the test binaries; built once if it is not there.
fn fake_child() -> PathBuf {
    static BUILD: Once = Once::new();
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let path = dir.join("fake-delegate-child");
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

struct Fake {
    _dir: tempfile::TempDir,
    binary: PathBuf,
    log: PathBuf,
}

impl Fake {
    fn new(version_line: &str, script: &str) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("script.txt");
        std::fs::write(&script_path, script).unwrap();
        let log = dir.path().join("log.txt");
        let binary = dir.path().join("claude");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version_line}'; exit 0; fi\n\
                 FAKE_CHILD_SCRIPT='{}' FAKE_CHILD_LOG='{}' exec '{}' \"$@\"\n",
                script_path.display(),
                log.display(),
                fake_child().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake {
            _dir: dir,
            binary,
            log,
        }
    }

    fn stdin(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("in: ").map(String::from))
            .collect()
    }
}

const VERSION: &str = "2.1.312 (Claude Code)";
const INIT: &str = r#"{"type":"system","subtype":"init","apiKeySource":"none","claude_code_version":"2.1.312","model":"claude-x","session_id":"sess-9"}"#;
const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.25,"result":"ok","usage":{"input_tokens":3,"output_tokens":5},"modelUsage":{},"permission_denials":[],"session_id":"sess-9"}"#;
const ASSISTANT: &str = r#"{"type":"assistant","message":{"id":"m1","model":"claude-x","role":"assistant","content":[{"type":"text","text":"Hello there"}]},"parent_tool_use_id":null,"session_id":"sess-9"}"#;
const TOOL: &str = r#"{"type":"assistant","message":{"id":"m2","model":"claude-x","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"cargo test"}}]},"parent_tool_use_id":null,"session_id":"sess-9"}"#;
const ASK: &str = r#"{"type":"control_request","request_id":"req_1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"cargo test"},"tool_use_id":"toolu_2","decision_reason":"run the tests"}}"#;
const ASK_PLAN: &str = r#"{"type":"control_request","request_id":"req_2","request":{"subtype":"can_use_tool","tool_name":"ExitPlanMode","input":{"plan":"1. do it"},"tool_use_id":"toolu_3"}}"#;

/// Answers `authorize` with a fixed answer and records the requests.
struct Gate {
    mode: Mode,
    answer: GateAnswer,
    seen: Mutex<Vec<GateRequest>>,
    confirmed: Mutex<Vec<(ApprovalKind, String)>>,
    yes: Option<bool>,
    /// Whether a `None` answer is the user's Esc.
    esc: bool,
}

impl Gate {
    fn new(answer: GateAnswer) -> Gate {
        Gate {
            mode: Mode::Ask,
            answer,
            seen: Mutex::new(Vec::new()),
            confirmed: Mutex::new(Vec::new()),
            yes: Some(true),
            esc: false,
        }
    }
}

#[async_trait]
impl ApprovalGate for Gate {
    fn mode(&self) -> Mode {
        self.mode
    }
    async fn authorize(&self, request: GateRequest) -> GateAnswer {
        self.seen.lock().unwrap().push(request);
        self.answer.clone()
    }
    async fn confirm(&self, kind: ApprovalKind, question: String) -> Option<bool> {
        self.confirmed.lock().unwrap().push((kind, question));
        self.yes
    }
    fn stopped(&self) -> bool {
        self.esc
    }
}

fn provider(fake: &Fake, auth: Auth) -> DelegatedProvider {
    DelegatedProvider::with_backend(
        BackendKind::Claude,
        "sonnet",
        DelegateSettings {
            claude_binary: Some(fake.binary.clone()),
            ..DelegateSettings::default()
        },
        auth,
        Arc::new(ClaudeBackend),
    )
}

fn turn(workspace: &Path) -> DelegatedTurn {
    DelegatedTurn {
        task: "fix it".into(),
        model: "sonnet".into(),
        model_id: "claude-code/sonnet".into(),
        mode: Mode::Ask,
        full_access_confirmed: false,
        workspace: workspace.to_path_buf(),
        resume: None,
        cost_baseline_usd: 0.0,
        instructions: Some("Use tabs.".into()),
        redactor: Default::default(),
        role: Role::Main,
        switch_reason: None,
    }
}

async fn run(
    p: &DelegatedProvider,
    gate: &Gate,
    turn: DelegatedTurn,
    cancel: CancellationToken,
) -> (DelegatedOutcome, Vec<AgentEvent>) {
    let delegate = p.delegated().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        delegate.run(turn, gate, &tx, cancel),
    )
    .await
    .expect("the run ended");
    drop(tx);
    let mut events = Vec::new();
    while let Some(e) = rx.recv().await {
        events.push(e);
    }
    (outcome, events)
}

fn api_key() -> ApiKey {
    ApiKey::new("sk-ant-test")
}

#[tokio::test]
async fn a_claude_turn_becomes_the_agents_events_and_an_outcome() {
    let script =
        format!("expect user\nemit {INIT}\nemit {ASSISTANT}\nemit {TOOL}\nemit {RESULT}\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let p = provider(&fake, Auth::Own);
    let gate = Gate::new(GateAnswer::Allow);
    let (outcome, events) = run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    assert_eq!(outcome.session_id.as_deref(), Some("sess-9"));
    assert_eq!(outcome.texts, ["Hello there"]);
    assert_eq!(outcome.tool_calls, 1);
    assert_eq!(outcome.estimated_usd, Some(0.25));
    assert_eq!(outcome.cost_total_usd, Some(0.25));
    assert_eq!(outcome.usage.output_tokens, 5);
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::AssistantMessage { content, model, .. }
            if content == "Hello there" && model == "claude-code/sonnet"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolCallRequested { name, .. } if name == "Bash"
    )));
    // The subscription run says which account pays, once.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Warning { message } if message.contains("Claude subscription")
    )));
    assert_eq!(p.delegated().unwrap().account(), AccountKind::Delegated);
}

#[tokio::test]
async fn an_approval_goes_to_the_gate_as_a_bash_action_and_the_answer_goes_back() {
    let script =
        format!("expect user\nemit {INIT}\nemit {TOOL}\nemit {ASK}\nexpect req_1\nemit {RESULT}\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let p = provider(&fake, Auth::Own);
    let gate = Gate::new(GateAnswer::Deny {
        reason: "denied: no".into(),
    });
    let (outcome, _) = run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    assert_eq!(outcome.reason, TurnEndReason::Completed);
    let seen = gate.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].tool, "bash");
    assert_eq!(seen[0].actions, [Action::Bash("cargo test".into())]);
    // The child was told the reason, and nothing was allowed.
    let answer = fake
        .stdin()
        .into_iter()
        .find(|l| l.contains("req_1"))
        .unwrap();
    assert!(
        answer.contains("deny") && answer.contains("denied: no"),
        "{answer}"
    );
}

#[tokio::test]
async fn exit_plan_mode_is_refused_with_a_note_and_never_reaches_the_gate() {
    let script =
        format!("expect user\nemit {INIT}\nemit {ASK_PLAN}\nexpect req_2\nemit {RESULT}\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let p = provider(&fake, Auth::Own);
    let gate = Gate::new(GateAnswer::Allow);
    run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    assert!(gate.seen.lock().unwrap().is_empty());
    let answer = fake
        .stdin()
        .into_iter()
        .find(|l| l.contains("req_2"))
        .unwrap();
    assert!(answer.contains("write the finished plan"), "{answer}");
}

#[tokio::test]
async fn interrupting_stops_the_child_and_the_turn_is_interrupted() {
    let script = format!("expect user\nemit {INIT}\nhang\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let p = provider(&fake, Auth::Own);
    let gate = Gate::new(GateAnswer::Allow);
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        stopper.cancel();
    });
    let (outcome, _) = run(&p, &gate, turn(ws.path()), cancel).await;
    assert_eq!(outcome.reason, TurnEndReason::Interrupted);
}

#[tokio::test]
async fn a_missing_program_ends_the_turn_with_why() {
    let ws = tempfile::tempdir().unwrap();
    let p = DelegatedProvider::with_backend(
        BackendKind::Claude,
        "sonnet",
        DelegateSettings {
            claude_binary: Some("/nonexistent/claude".into()),
            ..DelegateSettings::default()
        },
        Auth::Own,
        Arc::new(ClaudeBackend),
    );
    let gate = Gate::new(GateAnswer::Allow);
    let (outcome, events) = run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    assert_eq!(outcome.reason, TurnEndReason::Error);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message, .. } if message.starts_with("claude-code cannot be used: ")))
    );
}

// --- the approval mapping ---------------------------------------------------------------------

fn approval(
    tool: &str,
    kind: DelegateApprovalKind,
    arguments: serde_json::Value,
) -> DelegateApproval {
    DelegateApproval {
        id: "a1".into(),
        call_id: "c1".into(),
        tool: tool.into(),
        arguments,
        kind,
        reason: "why".into(),
        can_remember: true,
    }
}

#[test]
fn files_and_reads_map_to_the_engines_actions() {
    let ws = Path::new("/w");
    let write = approval(
        "Write",
        DelegateApprovalKind::FileChange {
            paths: vec!["src/a.rs".into()],
        },
        json!({"file_path": "src/a.rs", "content": "x"}),
    );
    let Step::Ask(r) = approve::map(BackendKind::Claude, &write, ws, Mode::Ask) else {
        panic!()
    };
    assert_eq!(r.tool, "write");
    assert_eq!(r.actions, [Action::Write("/w/src/a.rs".into())]);
    assert_eq!(r.arguments["content"], "x");
    let read = approval(
        "Read",
        DelegateApprovalKind::Tool,
        json!({"file_path": "/etc/hosts"}),
    );
    let Step::Ask(r) = approve::map(BackendKind::Claude, &read, ws, Mode::Ask) else {
        panic!()
    };
    assert_eq!(r.actions, [Action::Read("/etc/hosts".into())]);
    let fetch = approval(
        "WebFetch",
        DelegateApprovalKind::Tool,
        json!({"url": "https://x"}),
    );
    let Step::Ask(r) = approve::map(BackendKind::Claude, &fetch, ws, Mode::Ask) else {
        panic!()
    };
    assert!(
        r.actions.is_empty(),
        "no action the engine knows: always asked"
    );
    let cmd = approval(
        "command",
        DelegateApprovalKind::Command {
            command: "ls".into(),
            cwd: None,
        },
        json!({}),
    );
    let Step::Ask(r) = approve::map(BackendKind::Codex, &cmd, ws, Mode::Ask) else {
        panic!()
    };
    assert_eq!(r.actions, [Action::Bash("ls".into())]);
}

#[test]
fn a_codex_grant_root_is_checked_as_a_write_too_and_named_in_the_reason() {
    let change = approval(
        "file_change",
        DelegateApprovalKind::FileChange {
            paths: vec!["src/a.rs".into()],
        },
        json!({"paths": ["src/a.rs"], "grantRoot": "/etc"}),
    );
    let Step::Ask(r) = approve::map(BackendKind::Codex, &change, Path::new("/w"), Mode::Ask) else {
        panic!()
    };
    assert_eq!(
        r.actions,
        [
            Action::Write("/w/src/a.rs".into()),
            Action::Write("/etc".into())
        ]
    );
    assert!(r.reason.contains("/etc"), "{}", r.reason);
    // No grant root, as in every recorded request: nothing is added.
    let plain = approval(
        "file_change",
        DelegateApprovalKind::FileChange {
            paths: vec!["src/a.rs".into()],
        },
        json!({"paths": ["src/a.rs"], "grantRoot": null}),
    );
    let Step::Ask(r) = approve::map(BackendKind::Codex, &plain, Path::new("/w"), Mode::Ask) else {
        panic!()
    };
    assert_eq!(r.actions.len(), 1);
}

#[test]
fn codex_in_plan_and_read_only_modes_is_refused_and_claude_is_left_to_the_engine() {
    let cmd = approval(
        "command",
        DelegateApprovalKind::Command {
            command: "ls".into(),
            cwd: None,
        },
        json!({}),
    );
    for mode in [Mode::Plan, Mode::ReadOnly] {
        assert!(matches!(
            approve::map(BackendKind::Codex, &cmd, Path::new("/w"), mode),
            Step::Refuse(_)
        ));
        assert!(matches!(
            approve::map(BackendKind::Claude, &cmd, Path::new("/w"), mode),
            Step::Ask(_)
        ));
    }
    let exit = approval(
        "ExitPlanMode",
        DelegateApprovalKind::Tool,
        json!({"plan": "p"}),
    );
    assert_eq!(
        approve::map(BackendKind::Claude, &exit, Path::new("/w"), Mode::Plan),
        Step::Refuse(PLAN_NOTE.into())
    );
}

// --- the account ------------------------------------------------------------------------------

#[derive(Default)]
struct Store(Mutex<Option<bool>>);
impl OptInStore for Store {
    fn get(&self) -> Option<bool> {
        *self.0.lock().unwrap()
    }
    fn set(&self, yes: bool) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(yes);
        Ok(())
    }
}

#[tokio::test]
async fn the_first_use_asks_and_keeps_the_answer() {
    let store = Arc::new(Store::default());
    let p = DelegatedProvider::new(
        "claude-code",
        "sonnet",
        DelegateSettings {
            store: Some(store.clone()),
            ..DelegateSettings::default()
        },
        Some(api_key()),
    )
    .unwrap();
    let gate = Gate::new(GateAnswer::Allow);
    p.delegated().unwrap().prepare(&gate).await.unwrap();
    let asked = gate.confirmed.lock().unwrap();
    assert_eq!(asked[0].0, ApprovalKind::DelegateOptIn);
    assert!(asked[0].1.contains("never reads that program's tokens"));
    assert!(asked[0].1.contains("not settled"));
    assert_eq!(store.get(), Some(true));
    assert_eq!(p.delegated().unwrap().account(), AccountKind::Delegated);
}

#[tokio::test]
async fn a_no_uses_the_api_key_and_with_no_key_the_model_is_unusable() {
    let mut gate = Gate::new(GateAnswer::Allow);
    gate.yes = Some(false);
    let with_key = DelegatedProvider::new(
        "claude-code",
        "sonnet",
        DelegateSettings::default(),
        Some(api_key()),
    )
    .unwrap();
    with_key.delegated().unwrap().prepare(&gate).await.unwrap();
    assert_eq!(with_key.delegated().unwrap().account(), AccountKind::ApiKey);
    let without =
        DelegatedProvider::new("claude-code", "sonnet", DelegateSettings::default(), None).unwrap();
    let why = without
        .delegated()
        .unwrap()
        .prepare(&gate)
        .await
        .unwrap_err();
    assert_eq!(why, optin::REFUSED_NO_KEY);
}

#[tokio::test]
async fn nobody_to_ask_falls_back_to_the_key_without_storing_an_answer() {
    let store = Arc::new(Store::default());
    let mut gate = Gate::new(GateAnswer::Allow);
    gate.yes = None;
    let settings = DelegateSettings {
        store: Some(store.clone()),
        ..DelegateSettings::default()
    };
    let with_key =
        DelegatedProvider::new("claude-code", "sonnet", settings.clone(), Some(api_key())).unwrap();
    with_key.delegated().unwrap().prepare(&gate).await.unwrap();
    assert_eq!(with_key.delegated().unwrap().account(), AccountKind::ApiKey);
    assert_eq!(store.get(), None);
    let none = DelegatedProvider::new("claude-code", "sonnet", settings, None).unwrap();
    assert_eq!(
        none.delegated().unwrap().prepare(&gate).await.unwrap_err(),
        optin::NO_ACCOUNT
    );
}

#[test]
fn an_answered_opt_in_decides_at_once_and_codex_needs_none() {
    let settings = DelegateSettings {
        claude_subscription: Some(false),
        ..DelegateSettings::default()
    };
    let p = DelegatedProvider::new("claude-code", "sonnet", settings, None).unwrap();
    assert_eq!(p.unusable().as_deref(), Some(optin::REFUSED_NO_KEY));
    let codex =
        DelegatedProvider::new("codex", "gpt-5", DelegateSettings::default(), None).unwrap();
    assert!(codex.unusable().is_none());
    assert_eq!(codex.delegated().unwrap().account(), AccountKind::Delegated);
    assert_eq!(codex.delegated().unwrap().label(), "codex");
    assert!(DelegatedProvider::new("openai", "x", DelegateSettings::default(), None).is_none());
}

// --- resolving the ids ------------------------------------------------------------------------

mod resolve {
    use std::collections::BTreeMap;

    use harness_providers::{
        profiles,
        registry::{self, ResolveError, Secrets},
    };

    use super::*;

    struct Keys {
        key: Option<&'static str>,
        settings: DelegateSettings,
    }

    impl Secrets for Keys {
        fn env(&self, var: &str) -> Option<String> {
            (var == "ANTHROPIC_API_KEY")
                .then(|| self.key.map(String::from))
                .flatten()
        }
        fn delegate(&self) -> DelegateSettings {
            self.settings.clone()
        }
    }

    fn resolve(
        id: &str,
        key: Option<&'static str>,
        opt_in: Option<bool>,
    ) -> Result<(), ResolveError> {
        registry::resolve(
            id,
            &BTreeMap::new(),
            Keys {
                key,
                settings: DelegateSettings {
                    claude_subscription: opt_in,
                    ..DelegateSettings::default()
                },
            },
        )
        .map(|r| {
            assert_eq!(r.id, id);
            assert!(r.provider.delegated().is_some());
            assert!(r.api_key.is_none(), "the key stays inside the provider");
        })
    }

    #[test]
    fn both_providers_resolve_wherever_a_model_id_does() {
        resolve("claude-code/sonnet", None, None).expect("asked at the first turn");
        resolve("claude-code/opus", None, Some(true)).expect("opted in");
        resolve("claude-code/opus", Some("sk-ant-api"), Some(false)).expect("the key");
        resolve("codex/gpt-5-codex", None, None).expect("codex needs no opt-in");
    }

    #[test]
    fn a_declined_opt_in_with_no_key_says_why() {
        let Err(ResolveError::DelegateUnusable { provider, reason }) =
            resolve("claude-code/sonnet", None, Some(false))
        else {
            panic!("should be unusable")
        };
        assert_eq!(provider, "claude-code");
        assert!(reason.contains("harness auth add anthropic"), "{reason}");
    }

    #[test]
    fn a_subscription_token_is_not_a_key() {
        // Treated as no key: declined and no key is unusable.
        assert!(matches!(
            resolve("claude-code/sonnet", Some("sk-ant-oat01-abc"), Some(false)),
            Err(ResolveError::DelegateUnusable { .. })
        ));
    }

    #[test]
    fn the_models_have_a_window_for_the_status_line() {
        for (id, tokens) in [("claude-code/sonnet", 200_000), ("codex/gpt-5", 272_000)] {
            let profile = profiles::resolve(id, false, &BTreeMap::new());
            assert_eq!(profile.context_window, Some(tokens), "{id}");
            assert!(!profile.local);
        }
    }
}

#[tokio::test]
async fn esc_on_the_opt_in_leaves_it_unanswered_and_the_next_turn_asks_again() {
    let store = Arc::new(Store::default());
    let mut gate = Gate::new(GateAnswer::Allow);
    gate.yes = None;
    gate.esc = true;
    let settings = DelegateSettings {
        store: Some(store.clone()),
        ..DelegateSettings::default()
    };
    // There is a key: Esc must not pick it.
    let p = DelegatedProvider::new("claude-code", "sonnet", settings, Some(api_key())).unwrap();
    let delegate = p.delegated().unwrap();
    assert!(delegate.prepare(&gate).await.is_err());
    assert_eq!(
        delegate.account(),
        AccountKind::Delegated,
        "no account was chosen"
    );
    assert_eq!(store.get(), None);
    // The second turn asks again, and a "yes" then is used.
    gate.esc = false;
    gate.yes = Some(true);
    delegate.prepare(&gate).await.unwrap();
    assert_eq!(gate.confirmed.lock().unwrap().len(), 2);
    assert_eq!(store.get(), Some(true));
}

#[tokio::test]
async fn with_nobody_to_ask_the_key_is_used_and_the_run_says_so() {
    let script = format!("expect user\nemit {INIT}\nemit {ASSISTANT}\nemit {RESULT}\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let mut gate = Gate::new(GateAnswer::Allow);
    gate.yes = None;
    let p = provider(
        &fake,
        Auth::Undecided {
            key: Some(api_key()),
        },
    );
    p.delegated().unwrap().prepare(&gate).await.unwrap();
    let (_, events) = run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    let said = events.iter().any(|e| {
        matches!(e, AgentEvent::Warning { message } if message.contains("nobody could be asked"))
    });
    assert!(said, "{events:?}");
}

#[test]
fn only_codex_has_usage_windows_to_show() {
    let claude =
        DelegatedProvider::new("claude-code", "sonnet", DelegateSettings::default(), None).unwrap();
    assert!(claude.windows().is_none());
    let codex =
        DelegatedProvider::new("codex", "gpt-5", DelegateSettings::default(), None).unwrap();
    assert!(codex.windows().is_some());
}

#[tokio::test]
async fn text_streamed_and_never_closed_into_a_message_is_still_what_the_child_said() {
    let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half an answer"}},"session_id":"sess-9"}"#;
    let script = format!("expect user\nemit {INIT}\nemit {delta}\nemit {RESULT}\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let p = provider(&fake, Auth::Own);
    let gate = Gate::new(GateAnswer::Allow);
    let (outcome, _) = run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    assert_eq!(outcome.texts, ["half an answer"]);
}

#[tokio::test]
async fn an_answer_that_could_not_be_kept_is_said_not_claimed() {
    struct Broken;
    impl OptInStore for Broken {
        fn get(&self) -> Option<bool> {
            None
        }
        fn set(&self, _yes: bool) -> Result<(), String> {
            Err("the data directory is read-only".into())
        }
    }
    let script = format!("expect user\nemit {INIT}\nemit {ASSISTANT}\nemit {RESULT}\n");
    let fake = Fake::new(VERSION, &script);
    let ws = tempfile::tempdir().unwrap();
    let p = DelegatedProvider::with_backend(
        BackendKind::Claude,
        "sonnet",
        DelegateSettings {
            claude_binary: Some(fake.binary.clone()),
            store: Some(Arc::new(Broken)),
            ..DelegateSettings::default()
        },
        Auth::Undecided { key: None },
        Arc::new(ClaudeBackend),
    );
    let gate = Gate::new(GateAnswer::Allow);
    p.delegated().unwrap().prepare(&gate).await.unwrap();
    let (_, events) = run(&p, &gate, turn(ws.path()), CancellationToken::new()).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Warning { message } if message.contains("could not be kept") && message.contains("read-only")
        )),
        "{events:?}"
    );
}
