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

// Review F M7: the common secret names beyond the four endings, and the password in a URL.
#[test]
fn common_secret_names_and_the_passwords_in_urls_are_secrets() {
    let redactor = Redactor::default();
    redactor.add_env([
        ("PULUMI_CONFIG_PASSPHRASE", "passphrase-value-1"),
        ("MYSQL_PWD", "mysql-pwd-value-2"),
        ("SMTP_PASS", "smtp-pass-value-3"),
        ("GOOGLE_CREDENTIALS", "credentials-value-4"),
        ("API_KEYS", "api-keys-value-5"),
        ("GITHUB_TOKENS", "tokens-value-6"),
        ("VAULT_SECRETS", "secrets-value-7"),
        ("ADMIN_PASSWORDS", "passwords-value-8"),
        (
            "DATABASE_URL",
            "postgres://app:url-password-9@db.internal:5432/app",
        ),
        ("HTTPS_PROXY", "http://me:p%40ss-word-10@proxy:8080"),
        // Not secrets: the working directories, names that only end in the same letters, a URL
        // without a password, and values too short to tell from ordinary text.
        ("PWD", "/home/someone/project"),
        ("OLDPWD", "/home/someone/elsewhere"),
        ("COMPASS", "north-by-northwest"),
        ("HOMEPAGE_URL", "https://someone@example.com/some/path"),
        ("MAX_TOKENS", "4096"),
    ]);
    let secrets = [
        "passphrase-value-1",
        "mysql-pwd-value-2",
        "smtp-pass-value-3",
        "credentials-value-4",
        "api-keys-value-5",
        "tokens-value-6",
        "secrets-value-7",
        "passwords-value-8",
        "url-password-9",
        "p%40ss-word-10",
        "p@ss-word-10",
    ];
    for secret in secrets {
        assert_eq!(redactor.redact(secret), REDACTED, "{secret}");
    }
    for plain in [
        "/home/someone/project",
        "/home/someone/elsewhere",
        "north-by-northwest",
        "https://someone@example.com/some/path",
        "4096",
        "postgres://app:",
    ] {
        assert_eq!(redactor.redact(plain), plain);
    }
}

// Review F I1: a variable that is not UTF-8 is read, not a panic, and its value is a secret in
// its lossy form, the form the bash tool's output takes.
#[test]
fn environment_variables_that_are_not_utf8_are_read_lossily() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let redactor = Redactor::default();
    redactor.add_env([
        (
            OsString::from("LEGACY_TOKEN"),
            OsString::from_vec(b"caf\xe9-token-1234".to_vec()),
        ),
        (
            OsString::from_vec(b"CAF\xc9_SECRET".to_vec()),
            OsString::from("latin1-named-secret"),
        ),
        (
            OsString::from("LEGACY_NAME"),
            OsString::from_vec(b"caf\xe9-not-a-secret".to_vec()),
        ),
    ]);
    assert_eq!(
        redactor.redact("caf\u{fffd}-token-1234 latin1-named-secret caf\u{fffd}-not-a-secret"),
        format!("{REDACTED} {REDACTED} caf\u{fffd}-not-a-secret")
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
