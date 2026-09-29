use std::collections::{BTreeMap, HashMap};

use harness_config::config::{Protocol, ProviderConfig};
use harness_providers::registry::{ResolveError, configured_endpoints, local_endpoints, resolve};

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
