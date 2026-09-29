use std::collections::{BTreeMap, HashMap};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::credentials::CredentialError;
use harness_providers::registry::{
    ResolveError, Secrets, configured_endpoints, local_endpoints, resolve,
};

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

fn custom(name: &str, url: &str, key_env: Option<&str>) -> BTreeMap<String, ProviderConfig> {
    BTreeMap::from([(
        name.to_string(),
        ProviderConfig {
            protocol: Protocol::OpenaiChat,
            base_url: url.into(),
            api_key_env: key_env.map(String::from),
        },
    )])
}

#[test]
fn resolves_builtin_local_providers() {
    let r = resolve("ollama/qwen3:14b", &BTreeMap::new(), env(&[])).unwrap();
    assert_eq!(r.model, "qwen3:14b");
    assert_eq!(r.id, "ollama/qwen3:14b");
}

#[test]
fn splits_only_at_the_first_slash() {
    let r = resolve(
        "openrouter/qwen/qwen3-coder",
        &BTreeMap::new(),
        env(&[("OPENROUTER_API_KEY", "k")]),
    )
    .unwrap();
    assert_eq!(r.model, "qwen/qwen3-coder");
}

#[test]
fn reports_bad_ids_unknown_providers_and_missing_keys() {
    let none = BTreeMap::new();
    assert_eq!(
        resolve("qwen", &none, env(&[])).err(),
        Some(ResolveError::BadId("qwen".into()))
    );
    assert_eq!(
        resolve("ollama/", &none, env(&[])).err(),
        Some(ResolveError::BadId("ollama/".into()))
    );
    assert_eq!(
        resolve("nope/m", &none, env(&[])).err(),
        Some(ResolveError::UnknownProvider("nope".into()))
    );
    assert_eq!(
        resolve("openrouter/m", &none, env(&[])).err(),
        Some(ResolveError::MissingKey {
            provider: "openrouter".into(),
            profile: "default".into(),
            var: "OPENROUTER_API_KEY".into()
        })
    );
}

#[test]
fn configured_providers_resolve_and_override_builtins() {
    let providers = custom("ollama", "http://gpu-box:11434/v1", None);
    assert_eq!(
        resolve("ollama/m", &providers, env(&[])).unwrap().model,
        "m"
    );
    assert!(
        local_endpoints(&providers)
            .iter()
            .all(|e| e.provider != "ollama")
    );
}

#[test]
fn configured_endpoints_require_their_key() {
    let providers = custom("work", "https://llm.example/v1", Some("WORK_KEY"));
    assert!(configured_endpoints(&providers, env(&[])).is_empty());
    let with_key = configured_endpoints(&providers, env(&[("WORK_KEY", "k")]));
    assert_eq!(with_key.len(), 1);
    assert_eq!(with_key[0].api_key.as_deref(), Some("k"));
}

// Review Focus: the built-in openrouter provider needs a key but isn't in any user config, so it
// was invisible to `configured_endpoints` (and so to `harness models`) even with a key set.
#[test]
fn configured_endpoints_include_builtin_openrouter_when_its_key_is_set() {
    assert!(configured_endpoints(&BTreeMap::new(), env(&[])).is_empty());

    let found = configured_endpoints(&BTreeMap::new(), env(&[("OPENROUTER_API_KEY", "k")]));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].provider, "openrouter");
    assert_eq!(found[0].base_url, "https://openrouter.ai/api/v1");
    assert_eq!(found[0].api_key.as_deref(), Some("k"));
}

#[test]
fn configured_endpoints_prefer_the_users_own_openrouter_definition() {
    let providers = custom(
        "openrouter",
        "https://custom.example/v1",
        Some("OPENROUTER_API_KEY"),
    );
    let found = configured_endpoints(&providers, env(&[("OPENROUTER_API_KEY", "k")]));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].base_url, "https://custom.example/v1");
}

#[test]
fn local_endpoints_cover_the_three_servers() {
    let names: Vec<String> = local_endpoints(&BTreeMap::new())
        .into_iter()
        .map(|e| e.provider)
        .collect();
    assert_eq!(names, ["ollama", "lmstudio", "llamacpp"]);
}

#[test]
fn builtin_openai_speaks_the_responses_protocol_with_its_key() {
    let r = resolve(
        "openai/gpt-5",
        &BTreeMap::new(),
        env(&[("OPENAI_API_KEY", "k")]),
    )
    .unwrap();
    assert_eq!(r.model, "gpt-5");
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://api.openai.com/v1");
    assert_eq!(
        resolve("openai/gpt-5", &BTreeMap::new(), env(&[])).err(),
        Some(ResolveError::MissingKey {
            provider: "openai".into(),
            profile: "default".into(),
            var: "OPENAI_API_KEY".into()
        })
    );
    let found = configured_endpoints(&BTreeMap::new(), env(&[("OPENAI_API_KEY", "k")]));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].provider, "openai");
}

#[test]
fn configured_providers_may_speak_the_responses_protocol() {
    let mut providers = custom("azure", "https://x.example/openai/v1", Some("AZ_KEY"));
    providers.get_mut("azure").unwrap().protocol = Protocol::OpenaiResponses;
    let r = resolve("azure/gpt-5", &providers, env(&[("AZ_KEY", "k")])).unwrap();
    assert_eq!(r.protocol, Protocol::OpenaiResponses);
    assert_eq!(r.base_url, "https://x.example/openai/v1");
}

#[test]
fn builtin_anthropic_speaks_the_messages_protocol_with_its_key() {
    let r = resolve(
        "anthropic/claude-sonnet-4-5",
        &BTreeMap::new(),
        env(&[("ANTHROPIC_API_KEY", "sk-ant-api03-k")]),
    )
    .unwrap();
    assert_eq!(r.model, "claude-sonnet-4-5");
    assert_eq!(r.protocol, Protocol::AnthropicMessages);
    assert_eq!(r.base_url, "https://api.anthropic.com/v1");
    assert_eq!(
        resolve("anthropic/claude-sonnet-4-5", &BTreeMap::new(), env(&[])).err(),
        Some(ResolveError::MissingKey {
            provider: "anthropic".into(),
            profile: "default".into(),
            var: "ANTHROPIC_API_KEY".into()
        })
    );
    let found = configured_endpoints(
        &BTreeMap::new(),
        env(&[("ANTHROPIC_API_KEY", "sk-ant-api03-k")]),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].protocol, Protocol::AnthropicMessages);
}

/// Environment variables and stored keys, as the CLI gives them.
struct Keys {
    env: HashMap<String, String>,
    stored: HashMap<String, String>,
    /// The profile every provider uses.
    profile: String,
}

impl Keys {
    fn new(env: &[(&str, &str)], stored: &[(&str, &str)]) -> Keys {
        let map = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        Keys {
            env: map(env),
            stored: map(stored),
            profile: "default".into(),
        }
    }
}

impl Secrets for Keys {
    fn env(&self, var: &str) -> Option<String> {
        self.env.get(var).cloned()
    }

    fn profile(&self, _provider: &str) -> Result<String, CredentialError> {
        Ok(self.profile.clone())
    }

    fn stored(&self, provider: &str) -> Result<Option<String>, CredentialError> {
        Ok(self.stored.get(provider).cloned())
    }
}

// Spec: "Environment variable wins".
#[test]
fn the_environment_wins_over_a_stored_key() {
    let none = BTreeMap::new();
    let stored = Keys::new(&[], &[("openai", "sk-stored")]);
    let r = resolve("openai/gpt-5", &none, stored).unwrap();
    assert_eq!(r.api_key.as_deref(), Some("sk-stored"));
    let both = Keys::new(&[("OPENAI_API_KEY", "sk-env")], &[("openai", "sk-stored")]);
    let r = resolve("openai/gpt-5", &none, both).unwrap();
    assert_eq!(r.api_key.as_deref(), Some("sk-env"));
    let found = configured_endpoints(&none, Keys::new(&[], &[("openai", "sk-stored")]));
    assert_eq!(found[0].api_key.as_deref(), Some("sk-stored"));
}

// A local server has no key variable: nothing stored is looked up for it, so a request to it
// never reads the keychain.
#[test]
fn a_provider_without_a_key_variable_takes_no_stored_key() {
    let providers = custom("local", "http://127.0.0.1:8000/v1", None);
    let r = resolve("local/m", &providers, Keys::new(&[], &[("local", "k")])).unwrap();
    assert_eq!(r.api_key, None);
    let r = resolve(
        "ollama/m",
        &BTreeMap::new(),
        Keys::new(&[], &[("ollama", "k")]),
    )
    .unwrap();
    assert_eq!(r.api_key, None);
}

#[test]
fn a_missing_key_says_how_to_add_one() {
    let error = resolve("openai/gpt-5", &BTreeMap::new(), env(&[])).unwrap_err();
    assert_eq!(
        error.to_string(),
        "provider `openai` needs an API key: set $OPENAI_API_KEY or run `harness auth add openai`"
    );
}

// Review B, M4: `harness auth add <provider>` alone stores under `default`, which does not help
// a provider that uses another profile.
#[test]
fn a_missing_key_names_the_profile_in_use() {
    let mut keys = Keys::new(&[], &[]);
    keys.profile = "work".into();
    let error = resolve("openai/gpt-5", &BTreeMap::new(), keys).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("`harness auth add openai --profile work`"),
        "{error}"
    );
}

/// A credential store that cannot be read.
struct Damaged;

impl Secrets for Damaged {
    fn env(&self, _var: &str) -> Option<String> {
        None
    }

    fn stored(&self, _provider: &str) -> Result<Option<String>, CredentialError> {
        Err(CredentialError::Damaged {
            path: "/data/credentials.json".into(),
            at: " at line 1, column 2".into(),
            recovery: "fix it, or delete it",
        })
    }
}

// Review B, M3: a store that cannot be read says so, rather than that the key is missing (with
// advice to add one that would fail as well).
#[test]
fn a_store_that_cannot_be_read_is_reported_as_such() {
    let error = resolve("openai/gpt-5", &BTreeMap::new(), Damaged).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("/data/credentials.json"), "{text}");
    assert!(text.contains("fix it, or delete it"), "{text}");
    assert!(
        !matches!(error, ResolveError::MissingKey { .. }),
        "{error:?}"
    );
    // Listing models goes on without that provider.
    assert!(configured_endpoints(&BTreeMap::new(), Damaged).is_empty());
}

// Review B, M5: a Claude subscription token is never a valid key for any provider, so it is
// refused whatever the protocol, even behind a byte-order mark or spaces.
#[test]
fn claude_subscription_tokens_are_refused_for_every_provider() {
    let none = BTreeMap::new();
    for token in [
        "sk-ant-oat01-abc",
        "\u{feff}sk-ant-oat01-abc",
        " \tsk-ant-oat01-abc\n",
    ] {
        assert_eq!(
            resolve(
                "openrouter/some/model",
                &none,
                env(&[("OPENROUTER_API_KEY", token)])
            )
            .err(),
            Some(ResolveError::SubscriptionToken {
                provider: "openrouter".into()
            }),
            "{token:?}"
        );
        let stored = Keys::new(&[], &[("openai", token)]);
        assert!(
            matches!(
                resolve("openai/gpt-5", &none, stored),
                Err(ResolveError::SubscriptionToken { .. })
            ),
            "{token:?}"
        );
        let found = configured_endpoints(&none, env(&[("OPENROUTER_API_KEY", token)]));
        assert!(found.is_empty(), "{found:?}");
    }
    assert!(harness_providers::registry::is_claude_subscription_token(
        "\u{feff}sk-ant-oat01-abc"
    ));
}

// Spec: "A subscription token in the environment".
#[test]
fn claude_subscription_tokens_are_refused_wherever_they_come_from() {
    let none = BTreeMap::new();
    let refused = ResolveError::SubscriptionToken {
        provider: "anthropic".into(),
    };
    let from_env = env(&[("ANTHROPIC_API_KEY", "sk-ant-oat01-abc")]);
    assert_eq!(
        resolve("anthropic/claude-sonnet-4-5", &none, from_env).err(),
        Some(refused.clone())
    );
    let stored = Keys::new(&[], &[("anthropic", "sk-ant-oat01-abc")]);
    assert_eq!(
        resolve("anthropic/claude-sonnet-4-5", &none, stored).err(),
        Some(refused.clone())
    );
    assert!(refused.to_string().contains("API key"), "{refused}");
}

// Review C, I1: whatever the providers map holds, `chatgpt` is the signed-in account, and the
// sign-in stored under it is never sent as an API key.
#[test]
fn chatgpt_never_takes_the_key_path() {
    let mut providers = custom("chatgpt", "https://proxy.example/v1", Some("PROXY_KEY"));
    providers.get_mut("chatgpt").unwrap().protocol = Protocol::OpenaiResponses;
    let sign_in = r#"{"access_token":"AT-probe","refresh_token":"RT-probe","account_id":"acct"}"#;
    let stored = Keys::new(&[], &[("chatgpt", sign_in)]);
    let result = resolve("chatgpt/gpt-5.5", &providers, stored);
    assert!(
        result
            .as_ref()
            .is_err_and(|e| !e.to_string().contains("API key")),
        "{:?}",
        result.map(|r| r.api_key)
    );
    let found = configured_endpoints(&providers, Keys::new(&[], &[("chatgpt", sign_in)]));
    assert!(found.is_empty(), "{found:?}");
}

/// The error a request to `model_id`, resolved with `secrets`, ends with when the provider
/// answers `status`.
async fn refused(
    model_id: &str,
    protocol: Protocol,
    status: u16,
    secrets: impl Secrets,
) -> harness_core::provider::ProviderError {
    use futures::StreamExt;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).set_body_string(
            r#"{"error":{"message":"Incorrect API key provided: sk-mo****WXYZ","code":"invalid_api_key"}}"#,
        ))
        .mount(&server)
        .await;
    let name = model_id.split('/').next().unwrap();
    let providers = BTreeMap::from([(
        name.to_string(),
        ProviderConfig {
            protocol,
            base_url: format!("{}/v1", server.uri()),
            api_key_env: Some("MOCK_API_KEY".into()),
        },
    )]);
    let resolved = resolve(model_id, &providers, secrets).unwrap();
    let request = harness_core::message::ChatRequest {
        model: resolved.model.clone(),
        ..Default::default()
    };
    let first = resolved.provider.stream(request).next().await.unwrap();
    first.unwrap_err()
}

// Final review, M-3: a key the provider refuses is named by where it came from (the variable,
// which wins over a stored key, or the stored profile), with what fixes it; never by its value.
#[tokio::test]
async fn a_refused_key_says_which_key_it_was_and_how_to_replace_it() {
    use harness_core::provider::ProviderError;
    for protocol in [
        Protocol::OpenaiChat,
        Protocol::OpenaiResponses,
        Protocol::AnthropicMessages,
    ] {
        let from_env = Keys::new(&[("MOCK_API_KEY", "sk-mock-env-0123456789")], &[]);
        let error = refused("mock/m", protocol, 401, from_env).await;
        let ProviderError::KeyRefused { status, hint, body } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*status, 401);
        assert!(body.contains("invalid_api_key"), "{body}");
        for part in ["$MOCK_API_KEY", "unset", "`harness auth add mock`"] {
            assert!(hint.contains(part), "{part}: {hint}");
        }
        assert!(!error.is_retryable());
        let mut stored = Keys::new(&[], &[("mock", "sk-mock-stored-0123456789")]);
        stored.profile = "work".into();
        let error = refused("mock/m", protocol, 403, stored).await;
        let ProviderError::KeyRefused { status, hint, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*status, 403);
        for part in [
            "stored",
            "profile `work`",
            "`harness auth add mock --profile work`",
        ] {
            assert!(hint.contains(part), "{part}: {hint}");
        }
        assert!(!error.to_string().contains("sk-mock"), "{error}");
    }
    // Other statuses are what they were.
    let keys = Keys::new(&[("MOCK_API_KEY", "sk-mock-env-0123456789")], &[]);
    let error = refused("mock/m", Protocol::OpenaiChat, 404, keys).await;
    assert!(
        matches!(error, ProviderError::Http { status: 404, .. }),
        "{error:?}"
    );
}
