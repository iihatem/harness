use std::{collections::BTreeMap, sync::Arc};

use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{discovery::Endpoint, openai_chat::OpenAiChat, openai_responses::OpenAiResponses};

/// A provider usable without configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    pub protocol: Protocol,
    pub base_url: &'static str,
    /// The environment variable holding its API key, when it needs one.
    pub key_env: Option<&'static str>,
}

const fn builtin(
    name: &'static str,
    protocol: Protocol,
    base_url: &'static str,
    key_env: Option<&'static str>,
) -> Builtin {
    Builtin {
        name,
        protocol,
        base_url,
        key_env,
    }
}

/// Providers usable without configuration.
pub const BUILTIN_PROVIDERS: [Builtin; 5] = [
    builtin(
        "ollama",
        Protocol::OpenaiChat,
        "http://127.0.0.1:11434/v1",
        None,
    ),
    builtin(
        "lmstudio",
        Protocol::OpenaiChat,
        "http://127.0.0.1:1234/v1",
        None,
    ),
    builtin(
        "llamacpp",
        Protocol::OpenaiChat,
        "http://127.0.0.1:8080/v1",
        None,
    ),
    builtin(
        "openrouter",
        Protocol::OpenaiChat,
        "https://openrouter.ai/api/v1",
        Some("OPENROUTER_API_KEY"),
    ),
    builtin(
        "openai",
        Protocol::OpenaiResponses,
        "https://api.openai.com/v1",
        Some("OPENAI_API_KEY"),
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
    pub protocol: Protocol,
    pub base_url: String,
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
    } else if let Some(builtin) = BUILTIN_PROVIDERS.iter().find(|b| b.name == name) {
        (
            builtin.protocol,
            builtin.base_url.to_string(),
            builtin.key_env.map(String::from),
        )
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
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key)),
        Protocol::OpenaiResponses => Arc::new(OpenAiResponses::new(base_url.clone(), api_key)),
    };
    Ok(Resolved {
        provider,
        model: model.to_string(),
        id: model_id.to_string(),
        protocol,
        base_url,
    })
}

/// The three local servers, except any the user has redefined in config.
pub fn local_endpoints(providers: &BTreeMap<String, ProviderConfig>) -> Vec<Endpoint> {
    BUILTIN_PROVIDERS
        .iter()
        .filter(|b| LOCAL_PROVIDERS.contains(&b.name) && !providers.contains_key(b.name))
        .map(|b| Endpoint {
            provider: b.name.to_string(),
            base_url: b.base_url.to_string(),
            api_key: None,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present, plus any built-in
/// provider that needs a key (openrouter, openai) whose key is set and that the user hasn't
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<Endpoint> {
    let mut endpoints: Vec<Endpoint> = providers
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
        .collect();

    for builtin in BUILTIN_PROVIDERS {
        if LOCAL_PROVIDERS.contains(&builtin.name) || providers.contains_key(builtin.name) {
            continue;
        }
        if let Some(api_key) = builtin.key_env.and_then(&env).filter(|v| !v.is_empty()) {
            endpoints.push(Endpoint {
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
                api_key: Some(api_key),
            });
        }
    }
    endpoints
}
