use std::{collections::BTreeMap, sync::Arc};

use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{discovery::Endpoint, openai_chat::OpenAiChat};

/// Providers usable without configuration: (name, base URL, API-key environment variable).
pub const BUILTIN_PROVIDERS: [(&str, &str, Option<&str>); 4] = [
    ("ollama", "http://127.0.0.1:11434/v1", None),
    ("lmstudio", "http://127.0.0.1:1234/v1", None),
    ("llamacpp", "http://127.0.0.1:8080/v1", None),
    (
        "openrouter",
        "https://openrouter.ai/api/v1",
        Some("OPENROUTER_API_KEY"),
    ),
];

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("model id `{0}` must look like <provider>/<model>")]
    BadId(String),
    #[error("unknown provider `{0}`; define it under [providers.{0}] in config.toml")]
    UnknownProvider(String),
    #[error("provider `{provider}` needs an API key in ${var}")]
    MissingKey { provider: String, var: String },
}

/// A ready-to-use provider for one model id.
pub struct Resolved {
    pub provider: Arc<dyn Provider>,
    /// The model name sent to the provider (the id without its `<provider>/` prefix).
    pub model: String,
    /// The full `<provider>/<model>` id.
    pub id: String,
}

pub fn resolve(
    model_id: &str,
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Resolved, ResolveError> {
    let (name, model) = model_id
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    let (protocol, base_url, key_env) = if let Some(cfg) = providers.get(name) {
        (cfg.protocol, cfg.base_url.clone(), cfg.api_key_env.clone())
    } else if let Some((_, url, key)) = BUILTIN_PROVIDERS.iter().find(|(n, ..)| *n == name) {
        (Protocol::OpenaiChat, url.to_string(), key.map(String::from))
    } else {
        return Err(ResolveError::UnknownProvider(name.to_string()));
    };
    let api_key =
        match key_env {
            Some(var) => Some(env(&var).filter(|v| !v.is_empty()).ok_or(
                ResolveError::MissingKey {
                    provider: name.to_string(),
                    var,
                },
            )?),
            None => None,
        };
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url, api_key)),
    };
    Ok(Resolved {
        provider,
        model: model.to_string(),
        id: model_id.to_string(),
    })
}

/// The three local servers, except any the user has redefined in config.
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
        .iter()
        .filter(|(name, ..)| LOCAL_PROVIDERS.contains(name) && !providers.contains_key(*name))
        .map(|(name, url, _)| Endpoint {
            provider: name.to_string(),
            base_url: url.to_string(),
            api_key: None,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<Endpoint> {
    providers
        .iter()
        .filter_map(|(name, cfg)| {
            let api_key = match &cfg.api_key_env {
                Some(var) => Some(env(var).filter(|v| !v.is_empty())?),
                None => None,
            };
            Some(Endpoint {
                provider: name.clone(),
                base_url: cfg.base_url.clone(),
                api_key,
            })
        })
        .collect()
}
