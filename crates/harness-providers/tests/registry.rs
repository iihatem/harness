use std::collections::{BTreeMap, HashMap};

use harness_config::config::{Protocol, ProviderConfig};
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
        }
    }
}

impl Secrets for Keys {
    fn env(&self, var: &str) -> Option<String> {
        self.env.get(var).cloned()
    }

    fn stored(&self, provider: &str) -> Option<String> {
        self.stored.get(provider).cloned()
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
