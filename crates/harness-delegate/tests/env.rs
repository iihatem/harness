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

fn env_from(
    kind: BackendKind,
    auth: &AuthMode,
    names: &[&str],
) -> std::collections::BTreeMap<String, String> {
    let parent = names
        .iter()
        .map(|n| (OsString::from(*n), OsString::from("v")))
        .chain([("PATH".into(), "/usr/bin".into())]);
    child_env(kind, auth, parent)
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// What Claude Code itself sets in its children's environment, plus every provider, route and
/// credential variable its binary reads (2.1.312).
const CLAUDE_SIDE: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_SOME_VARIABLE_THAT_DOES_NOT_EXIST_YET",
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
    "CLAUDE_CODE_API_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_GATEWAY",
    "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
    "CLAUDE_CODE_SKIP_VERTEX_AUTH",
    "CLAUDE_CODE_SOMETHING_NEW",
    "AWS_BEARER_TOKEN_BEDROCK",
];

const CODEX_SIDE: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORG_ID",
    "OPENAI_IDENTITY_TOKEN_FILE",
    "OPENAI_FEDERATION_RULE_ID",
    "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_GITHUB_PERSONAL_ACCESS_TOKEN",
    "CODEX_SOMETHING_NEW",
];

/// Settings that are the user's own, not a credential or a route: they must survive.
const KEPT: &[&str] = &[
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "CODEX_CA_CERTIFICATE",
    "CLAUDE_CONFIG_DIR",
    "HTTPS_PROXY",
    "SSL_CERT_FILE",
];

#[test]
fn no_anthropic_or_claude_code_variable_reaches_a_claude_child_in_either_mode() {
    let names: Vec<&str> = CLAUDE_SIDE.iter().chain(KEPT).copied().collect();
    for auth in [
        AuthMode::Subscription,
        AuthMode::ApiKey(ApiKey::new("sk-callers")),
    ] {
        let env = env_from(BackendKind::Claude, &auth, &names);
        for name in env.keys() {
            assert!(
                !name.starts_with("ANTHROPIC_") || name == "ANTHROPIC_API_KEY",
                "{name} reached claude"
            );
            assert!(
                !name.starts_with("CLAUDE_CODE_") && name != "CLAUDECODE",
                "{name} reached claude"
            );
            assert_ne!(name, "AWS_BEARER_TOKEN_BEDROCK");
        }
        // Only the caller's own key, and only in API-key mode.
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            matches!(auth, AuthMode::ApiKey(_)).then_some("sk-callers")
        );
        for kept in ["CLAUDE_CONFIG_DIR", "HTTPS_PROXY", "SSL_CERT_FILE"] {
            assert!(env.contains_key(kept), "{kept} is the user's own setting");
        }
    }
}

#[test]
fn no_codex_or_openai_credential_or_route_reaches_a_codex_child_but_its_home_does() {
    let names: Vec<&str> = CODEX_SIDE.iter().chain(KEPT).copied().collect();
    for auth in [
        AuthMode::Subscription,
        AuthMode::ApiKey(ApiKey::new("sk-callers")),
    ] {
        let env = env_from(BackendKind::Codex, &auth, &names);
        for name in env.keys() {
            assert!(
                !name.starts_with("OPENAI_") || name == "OPENAI_API_KEY",
                "{name} reached codex"
            );
            if let Some(rest) = name.strip_prefix("CODEX_") {
                assert!(
                    ["HOME", "SQLITE_HOME", "CA_CERTIFICATE"].contains(&rest),
                    "{name} reached codex"
                );
            }
        }
        assert_eq!(
            env.get("OPENAI_API_KEY").map(String::as_str),
            matches!(auth, AuthMode::ApiKey(_)).then_some("sk-callers")
        );
        for kept in ["CODEX_HOME", "CODEX_CA_CERTIFICATE", "HTTPS_PROXY"] {
            assert!(env.contains_key(kept), "{kept} must reach codex");
        }
    }
}

#[test]
fn each_child_gets_none_of_the_other_agents_variables() {
    let claude = env_from(BackendKind::Claude, &AuthMode::Subscription, CODEX_SIDE);
    assert!(claude.keys().all(|k| k == "PATH"), "{claude:?}");
    let codex = env_from(BackendKind::Codex, &AuthMode::Subscription, CLAUDE_SIDE);
    assert!(codex.keys().all(|k| k == "PATH"), "{codex:?}");
}
