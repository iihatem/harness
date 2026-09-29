//! Secrets stay out of what harness writes (session files, tool-output files), while the model
//! still sees tool output as it is.

mod common;

use std::sync::Arc;

use common::*;
use harness_core::agent::NonInteractive;
use harness_core::message::Message;
use harness_core::permission::Mode;
use harness_core::redact::{REDACTED, Redactor};
use harness_core::session::{EntryKind, Session};
use harness_core::testing::{MockProvider, Script};
use serde_json::json;

const KEY: &str = "sk-canary-0123456789abcdef";

#[test]
fn known_secrets_are_replaced_wherever_they_appear() {
    let redactor = Redactor::default();
    redactor.add(KEY);
    redactor.add("short");
    assert_eq!(
        redactor.redact(&format!("a {KEY} b {KEY}")),
        format!("a {REDACTED} b {REDACTED}")
    );
    // Too short to tell apart from ordinary text.
    assert_eq!(redactor.redact("short text"), "short text");
    // A secret that contains another is replaced whole.
    redactor.add("sk-canary-0123");
    assert_eq!(redactor.redact(KEY), REDACTED);
    assert!(!format!("{redactor:?}").contains("canary"));
}

#[test]
fn a_secret_is_found_in_its_json_escaped_form_too() {
    let redactor = Redactor::default();
    let secret = r#"pa"ss\word-2024"#;
    redactor.add(secret);
    let line = serde_json::to_string(&json!({"content": format!("x {secret} y")})).unwrap();
    let redacted = redactor.redact(&line);
    assert!(!redacted.contains("ss\\\\word"), "{redacted}");
    assert!(redacted.contains(REDACTED), "{redacted}");
}

#[test]
fn secret_looking_environment_variables_are_secrets() {
    let redactor = Redactor::default();
    redactor.add_env(
        [
            ("OPENAI_API_KEY", "sk-proj-aaaaaaaaaaaa"),
            ("GITHUB_TOKEN", "ghp_bbbbbbbbbbbbbbbb"),
            ("AWS_SECRET_ACCESS_KEY", "cccccccccccccccccc"),
            ("DB_PASSWORD", "dddddddddddd"),
            ("client_secret", "eeeeeeeeeeee"),
            ("HOME", "/home/someone"),
            ("SHORT_TOKEN", "abc"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string())),
    );
    let text = "sk-proj-aaaaaaaaaaaa ghp_bbbbbbbbbbbbbbbb cccccccccccccccccc dddddddddddd eeeeeeeeeeee /home/someone abc";
    assert_eq!(
        redactor.redact(text),
        format!("{REDACTED} {REDACTED} {REDACTED} {REDACTED} {REDACTED} /home/someone abc")
    );
}

#[test]
fn session_files_hold_no_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let redactor = Arc::new(Redactor::default());
    redactor.add(KEY);
    let mut session = Session::create(&dir.path().join("sessions"), dir.path());
    session.set_redactor(redactor);
    session.append(EntryKind::Message {
        message: Message::User {
            content: format!("my key is {KEY}"),
        },
        display: None,
        note: false,
    });
    let saved = std::fs::read_to_string(session.path().unwrap()).unwrap();
    assert!(!saved.contains(KEY), "{saved}");
    assert!(saved.contains(&format!("my key is {REDACTED}")), "{saved}");
    // In memory, the conversation is as it was.
    assert!(
        session
            .messages()
            .iter()
            .any(|(_, m)| matches!(m, Message::User { content } if content.contains(KEY)))
    );
}

// Spec: "A command prints the environment", in the core: the tool-output file and the session
// file hold no key, and the model still gets the output as it is.
#[tokio::test]
async fn tool_output_files_and_sessions_hold_no_secrets_but_the_model_sees_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let long = format!(
        "{}\nOPENAI_API_KEY={KEY}\n{}",
        "a".repeat(8_000),
        "b".repeat(8_000)
    );
    let provider = MockProvider::new(vec![
        Script::tool_call("c1", "echo", json!({"text": long})),
        Script::text("done"),
    ]);
    let redactor = Arc::new(Redactor::default());
    redactor.add(KEY);
    let sessions = dir.path().join("sessions");
    let mut agent = agent(
        provider.clone(),
        Mode::Auto,
        Arc::new(NonInteractive),
        dir.path(),
    )
    .with_session(Session::create(&sessions, dir.path()))
    .with_redactor(redactor);
    agent.config_mut().output_limit = 4_000;
    run(&mut agent, "go").await;
    let spilled = std::fs::read_to_string(dir.path().join(".spill/c1.txt")).unwrap();
    assert!(!spilled.contains(KEY));
    assert!(spilled.contains(&format!("OPENAI_API_KEY={REDACTED}")));
    for file in std::fs::read_dir(&sessions).unwrap() {
        let saved = std::fs::read_to_string(file.unwrap().path()).unwrap();
        assert!(!saved.contains(KEY), "{saved}");
    }
    // The call's arguments carried the key to the model's own request, as it wrote them.
    let requests = provider.requests();
    assert!(
        serde_json::to_string(&requests[1].messages)
            .unwrap()
            .contains(KEY)
    );
}
