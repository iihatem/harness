use std::{collections::BTreeMap, sync::Arc};

use crate::credentials::Credentials;

use harness_config::config::{Protocol, ProviderConfig};
use harness_core::provider::Provider;

use crate::{
    anthropic_messages::AnthropicMessages, discovery::Endpoint, openai_chat::OpenAiChat,
    openai_responses::OpenAiResponses,
};

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
pub const BUILTIN_PROVIDERS: [Builtin; 7] = [
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
    builtin(
        "anthropic",
        Protocol::AnthropicMessages,
        "https://api.anthropic.com/v1",
        Some("ANTHROPIC_API_KEY"),
    ),
    // Signed in with `harness login chatgpt`, not a key.
    builtin(
        "chatgpt",
        Protocol::OpenaiResponses,
        "https://chatgpt.com/backend-api/codex",
        None,
    ),
];

/// The built-in provider a ChatGPT account answers for.
pub const CHATGPT: &str = "chatgpt";

const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("model id `{0}` must look like <provider>/<model>")]
    BadId(String),
    #[error("unknown provider `{0}`; define it under [providers.{0}] in config.toml")]
    UnknownProvider(String),
    #[error(
        "provider `{provider}` needs an API key: set ${var} or run `harness auth add {provider}`"
    )]
    MissingKey { provider: String, var: String },
    #[error(
        "the key for `{provider}` is a Claude subscription token, which only Claude Code may use; harness needs an Anthropic API key (from console.anthropic.com)"
    )]
    SubscriptionToken { provider: String },
    #[error("not signed in to {provider} (profile {profile}); run `{}`", login_command(.provider, .profile))]
    NotSignedIn { provider: String, profile: String },
    #[error("this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)")]
    SignInUnavailable,
}

/// The command that signs in to `provider` under `profile`.
fn login_command(provider: &str, profile: &str) -> String {
    if profile == crate::credentials::DEFAULT_PROFILE {
        format!("harness login {provider}")
    } else {
        format!("harness login {provider} --profile {profile}")
    }
}

/// Where API keys come from: environment variables, and keys stored with `harness auth add`.
/// A closure over environment variables is a `Secrets` with nothing stored.
pub trait Secrets {
    fn env(&self, var: &str) -> Option<String>;
    /// The key stored for `provider`'s active profile.
    fn stored(&self, _provider: &str) -> Option<String> {
        None
    }

    /// The credential store, for providers that sign in (`chatgpt`).
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
    fn env(&self, var: &str) -> Option<String> {
        self(var)
    }
}

/// Whether `key` is a Claude subscription (OAuth) token rather than an API key.
pub fn is_claude_subscription_token(key: &str) -> bool {
    key.trim_start().starts_with("sk-ant-oat")
}

/// The key for provider `name`, whose key is in the environment variable `key_env`: that
/// variable, else the stored key. A provider without a key variable takes no key, and nothing
/// stored is looked up for it.
fn api_key(name: &str, key_env: Option<&str>, secrets: &impl Secrets) -> Option<String> {
    let var = key_env?;
    secrets
        .env(var)
        .filter(|v| !v.is_empty())
        .or_else(|| secrets.stored(name).filter(|v| !v.is_empty()))
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
    /// The API key requests carry, if any.
    pub api_key: Option<String>,
}

/// Leaves the key out, so that it never reaches a log or an error message.
impl std::fmt::Debug for Resolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

pub fn resolve(
    model_id: &str,
    providers: &BTreeMap<String, ProviderConfig>,
    secrets: impl Secrets,
) -> Result<Resolved, ResolveError> {
    let (name, model) = model_id
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| ResolveError::BadId(model_id.to_string()))?;
    if name == CHATGPT && !providers.contains_key(name) {
        return chatgpt(model_id, model, &secrets);
    }
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
    let api_key = api_key(name, key_env.as_deref(), &secrets);
    if let (None, Some(var)) = (&api_key, key_env) {
        return Err(ResolveError::MissingKey {
            provider: name.to_string(),
            var,
        });
    }
    // Claude Free/Pro/Max credentials may only be used by Claude Code itself.
    if protocol == Protocol::AnthropicMessages
        && api_key.as_deref().is_some_and(is_claude_subscription_token)
    {
        return Err(ResolveError::SubscriptionToken {
            provider: name.to_string(),
        });
    }
    let provider: Arc<dyn Provider> = match protocol {
        Protocol::OpenaiChat => Arc::new(OpenAiChat::new(base_url.clone(), api_key.clone())),
        Protocol::OpenaiResponses => {
            Arc::new(OpenAiResponses::new(base_url.clone(), api_key.clone()))
        }
        Protocol::AnthropicMessages => {
            Arc::new(AnthropicMessages::new(base_url.clone(), api_key.clone()))
        }
    };
    Ok(Resolved {
        provider,
        model: model.to_string(),
        id: model_id.to_string(),
        protocol,
        base_url,
        api_key,
    })
}

/// A `chatgpt/*` model, for the account signed in under the provider's active profile.
/// `HARNESS_CHATGPT_ISSUER` and `HARNESS_CHATGPT_BASE_URL` point sign-in and requests elsewhere,
/// for tests.
#[cfg(feature = "chatgpt-login")]
fn chatgpt(model_id: &str, model: &str, secrets: &impl Secrets) -> Result<Resolved, ResolveError> {
    use crate::chatgpt::{
        auth::{BASE_URL, ChatGptAuth},
        oauth::{ISSUER, OAuth},
    };
    let credentials = secrets.credentials();
    let profile = credentials
        .as_ref()
        .and_then(|c| c.active_profile(CHATGPT).ok())
        .unwrap_or_else(|| crate::credentials::DEFAULT_PROFILE.to_string());
    let not_signed_in = || ResolveError::NotSignedIn {
        provider: CHATGPT.to_string(),
        profile: profile.clone(),
    };
    let credentials = credentials.ok_or_else(not_signed_in)?;
    let issuer = secrets
        .env("HARNESS_CHATGPT_ISSUER")
        .unwrap_or_else(|| ISSUER.to_string());
    let auth = ChatGptAuth::load(credentials, &profile, OAuth::new(&issuer))
        .ok()
        .flatten()
        .ok_or_else(not_signed_in)?;
    let base_url = secrets
        .env("HARNESS_CHATGPT_BASE_URL")
        .unwrap_or_else(|| BASE_URL.to_string());
    Ok(Resolved {
        provider: Arc::new(OpenAiResponses::chatgpt(base_url.clone(), Arc::new(auth))),
        model: model.to_string(),
        id: model_id.to_string(),
        protocol: Protocol::OpenaiResponses,
        base_url,
        api_key: None,
    })
}

#[cfg(not(feature = "chatgpt-login"))]
fn chatgpt(
    _model_id: &str,
    _model: &str,
    _secrets: &impl Secrets,
) -> Result<Resolved, ResolveError> {
    Err(ResolveError::SignInUnavailable)
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
            protocol: b.protocol,
        })
        .collect()
}

/// Configured providers whose API key (if one is required) is present, plus any built-in
/// provider that needs a key (openrouter, openai, anthropic) whose key is set and that the user hasn't
/// redefined under `[providers.<name>]`.
pub fn configured_endpoints(
    providers: &BTreeMap<String, ProviderConfig>,
    secrets: impl Secrets,
) -> Vec<Endpoint> {
    let mut endpoints: Vec<Endpoint> = providers
        .iter()
        .filter_map(|(name, cfg)| {
            let api_key = api_key(name, cfg.api_key_env.as_deref(), &secrets);
            if cfg.api_key_env.is_some() && api_key.is_none() {
                return None;
            }
            Some(Endpoint {
                provider: name.clone(),
                base_url: cfg.base_url.clone(),
                api_key,
                protocol: cfg.protocol,
            })
        })
        .collect();

    for builtin in BUILTIN_PROVIDERS {
        if LOCAL_PROVIDERS.contains(&builtin.name) || providers.contains_key(builtin.name) {
            continue;
        }
        if builtin.key_env.is_none() {
            continue;
        }
        if let Some(api_key) = api_key(builtin.name, builtin.key_env, &secrets) {
            endpoints.push(Endpoint {
                provider: builtin.name.to_string(),
                base_url: builtin.base_url.to_string(),
                api_key: Some(api_key),
                protocol: builtin.protocol,
            });
        }
    }
    // A Claude subscription token is never sent anywhere, not even to list models.
    endpoints.retain(|e| {
        e.protocol != Protocol::AnthropicMessages
            || !e
                .api_key
                .as_deref()
                .is_some_and(is_claude_subscription_token)
    });
    endpoints
}
