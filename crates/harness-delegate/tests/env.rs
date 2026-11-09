use std::{collections::BTreeMap, ffi::OsString};

use harness_delegate::{
    BackendKind,
    env::{ApiKey, AuthMode, child_env},
};

fn parent() -> Vec<(OsString, OsString)> {
    [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/u"),
        ("ANTHROPIC_API_KEY", "sk-ant-parent"),
        ("ANTHROPIC_AUTH_TOKEN", "tok-parent"),
        ("ANTHROPIC_BASE_URL", "https://proxy.example"),
        ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ("OPENAI_API_KEY", "sk-openai-parent"),
        ("CODEX_API_KEY", "codex-parent"),
        ("OPENROUTER_API_KEY", "sk-or-parent"),
        ("GEMINI_API_KEY", "gem-parent"),
        ("GITHUB_TOKEN", "ghp_keepme"),
        ("HTTPS_PROXY", "http://proxy:3128"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}

fn env_of(kind: BackendKind, auth: &AuthMode) -> BTreeMap<String, String> {
    child_env(kind, auth, parent())
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

#[test]
fn claude_subscription_mode_removes_every_key_and_route_variable() {
    let env = env_of(BackendKind::Claude, &AuthMode::Subscription);
    for gone in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_USE_BEDROCK",
    ] {
        assert!(!env.contains_key(gone), "{gone} must not reach the child");
    }
    assert_eq!(env["PATH"], "/usr/bin");
    assert_eq!(env["HOME"], "/home/u");
    assert_eq!(env["HTTPS_PROXY"], "http://proxy:3128");
}

#[test]
fn claude_api_key_mode_passes_the_callers_key_and_not_the_parents() {
    let env = env_of(
        BackendKind::Claude,
        &AuthMode::ApiKey(ApiKey::new("sk-ant-callers")),
    );
    assert_eq!(env["ANTHROPIC_API_KEY"], "sk-ant-callers");
    assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
    assert!(!env.contains_key("CLAUDE_CODE_USE_BEDROCK"));
}

#[test]
fn other_providers_keys_never_reach_either_child() {
    for kind in [BackendKind::Claude, BackendKind::Codex] {
        for auth in [
            AuthMode::Subscription,
            AuthMode::ApiKey(ApiKey::new("key-value-123")),
        ] {
            let env = env_of(kind, &auth);
            for gone in ["OPENROUTER_API_KEY", "GEMINI_API_KEY"] {
                assert!(!env.contains_key(gone), "{gone} reached {kind:?}");
            }
        }
    }
}

#[test]
fn codex_subscription_mode_removes_its_key_variables_and_keeps_the_rest() {
    let env = env_of(BackendKind::Codex, &AuthMode::Subscription);
    assert!(!env.contains_key("OPENAI_API_KEY"));
    assert!(!env.contains_key("CODEX_API_KEY"));
    assert_eq!(env["GITHUB_TOKEN"], "ghp_keepme");
    assert_eq!(env["PATH"], "/usr/bin");
}

#[test]
fn codex_api_key_mode_sets_the_key() {
    let env = env_of(
        BackendKind::Codex,
        &AuthMode::ApiKey(ApiKey::new("sk-openai-callers")),
    );
    assert_eq!(env["OPENAI_API_KEY"], "sk-openai-callers");
    assert!(!env.contains_key("CODEX_API_KEY"));
}

#[test]
fn an_api_key_is_never_shown_by_debug() {
    let shown = format!(
        "{:?}",
        AuthMode::ApiKey(ApiKey::new("sk-very-secret-value"))
    );
    assert!(!shown.contains("sk-very-secret-value"), "{shown}");
}
