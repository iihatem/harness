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

#[test]
fn local_endpoints_cover_the_three_servers() {
    let names: Vec<String> = local_endpoints(&BTreeMap::new())
        .into_iter()
        .map(|e| e.provider)
        .collect();
    assert_eq!(names, ["ollama", "lmstudio", "llamacpp"]);
}
