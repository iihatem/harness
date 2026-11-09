mod common;

use std::time::Duration;

use common::Fake;
use harness_delegate::{
    DelegateError,
    models::{self, ListOptions, claude_aliases, codex_models, parse_claude_aliases},
};
use serde_json::json;

fn options(fake: &Fake) -> ListOptions {
    ListOptions {
        binary: Some(fake.binary.clone()),
        parent_env: Some(vec![(
            "PATH".into(),
            std::env::var_os("PATH").unwrap_or_default(),
        )]),
        timeout: Duration::from_secs(10),
        ..ListOptions::default()
    }
}

const CLAUDE_HELP: &str = "Usage: claude [options] [prompt]\n\nOptions:\n  --effort <level>   Effort\n  --model <model>    Model for the current session. Provide an alias for the latest model (e.g. 'sonnet' or 'opus') or a model's full name (e.g. 'claude-sonnet-4-6').\n  --fallback-model <model>  Enable automatic fallback to specified model when default model is overloaded (e.g. 'haiku')\n";

#[test]
fn claude_aliases_are_the_quoted_words_of_the_model_option_only() {
    assert_eq!(parse_claude_aliases(CLAUDE_HELP), ["sonnet", "opus"]);
    assert!(parse_claude_aliases("nothing here").is_empty());
}

#[test]
fn codex_models_skip_hidden_ones_and_odd_names() {
    let result = json!({"data": [
        {"id": "a", "model": "gpt-5-codex", "hidden": false},
        {"id": "b", "model": "gpt-5.1", "isDefault": true},
        {"id": "c", "model": "secret", "hidden": true},
        {"id": "-rf", "model": "-rf"},
        {"id": "only-id"},
    ]});
    assert_eq!(
        models::parse_codex_models(&result),
        ["gpt-5-codex", "gpt-5.1", "only-id"]
    );
}

#[tokio::test]
async fn claude_aliases_come_from_help() {
    let script: String = CLAUDE_HELP.lines().map(|l| format!("emit {l}\n")).collect();
    let fake = Fake::new("claude", "2.1.312 (Claude Code)", &script);
    let aliases = claude_aliases(&options(&fake)).await.unwrap();
    assert_eq!(aliases, ["sonnet", "opus"]);
    assert_eq!(fake.argv(), ["--help"]);
}

#[tokio::test]
async fn claude_with_no_aliases_in_its_help_lists_the_usual_three() {
    let fake = Fake::new("claude", "2.1.312 (Claude Code)", "emit Usage: claude\n");
    let aliases = claude_aliases(&options(&fake)).await.unwrap();
    assert_eq!(aliases, ["sonnet", "opus", "haiku"]);
}

#[tokio::test]
async fn codex_models_are_listed_through_the_app_server() {
    let script = "expect \"method\":\"initialize\"\n\
        emit {\"id\":1,\"result\":{\"userAgent\":\"codex/0.154.0\"}}\n\
        expect \"method\":\"initialized\"\n\
        expect \"method\":\"model/list\"\n\
        emit {\"id\":2,\"result\":{\"data\":[{\"id\":\"x\",\"model\":\"gpt-5-codex\"},{\"id\":\"y\",\"model\":\"gpt-5.1\",\"hidden\":true}],\"nextCursor\":\"p2\"}}\n\
        expect \"cursor\":\"p2\"\n\
        emit {\"id\":3,\"result\":{\"data\":[{\"id\":\"z\",\"model\":\"gpt-5.1-codex-mini\"}],\"nextCursor\":null}}\n\
        until-eof\n";
    let fake = Fake::new("codex", "codex-cli 0.154.0", script);
    let models = codex_models(&options(&fake)).await.unwrap();
    assert_eq!(models, ["gpt-5-codex", "gpt-5.1-codex-mini"]);
}

#[tokio::test]
async fn codex_model_list_error_lists_nothing_and_a_missing_binary_says_so() {
    let script = "expect \"method\":\"initialize\"\n\
        emit {\"id\":1,\"result\":{}}\nexpect \"method\":\"model/list\"\n\
        emit {\"id\":2,\"error\":{\"code\":-1,\"message\":\"no\"}}\nuntil-eof\n";
    let fake = Fake::new("codex", "codex-cli 0.154.0", script);
    assert!(codex_models(&options(&fake)).await.unwrap().is_empty());
    let none = ListOptions {
        path_var: Some("/nonexistent".into()),
        ..ListOptions::default()
    };
    assert!(matches!(
        codex_models(&none).await,
        Err(DelegateError::NotFound("codex"))
    ));
}

#[tokio::test]
async fn an_untested_version_is_refused_unless_allowed() {
    let fake = Fake::new("codex", "codex-cli 0.200.0", "until-eof\n");
    assert!(matches!(
        codex_models(&options(&fake)).await,
        Err(DelegateError::VersionOutOfRange { .. })
    ));
}
