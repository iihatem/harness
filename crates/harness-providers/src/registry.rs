use std::{collections::BTreeMap, sync::Arc};

use harness_core::redact::Redactor;

use crate::credentials::{CredentialError, Credentials, DEFAULT_PROFILE};

use futures::StreamExt;
use harness_config::config::{Protocol, ProviderConfig};
use harness_core::{
    message::ChatRequest,
    provider::{Provider, ProviderError, ProviderStream},
};

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

/// The local model servers harness finds on its own.
pub const LOCAL_PROVIDERS: [&str; 3] = ["ollama", "lmstudio", "llamacpp"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("model id `{0}` must look like <provider>/<model>")]
    BadId(String),
    #[error("unknown provider `{0}`; define it under [providers.{0}] in config.toml")]
    UnknownProvider(String),
    #[error(
        "provider `{provider}` needs an API key: set ${var} or run `{}`", auth_add_command(.provider, .profile)
    )]
    MissingKey {
        provider: String,
        /// The profile the provider uses, which the key is looked for under.
        profile: String,
        var: String,
    },
    /// The credential store could not be read: the file, what is wrong, and how to recover.
    #[error("cannot read the stored credentials for `{provider}`: {message}")]
    Store { provider: String, message: String },
    #[error(
        "the key for `{provider}` is a Claude subscription token, which only Claude Code may use; harness needs an Anthropic API key (from console.anthropic.com)"
    )]
    SubscriptionToken { provider: String },
    #[error("not signed in to {provider} (profile {profile}); run `{}`", login_command(.provider, .profile))]
    NotSignedIn { provider: String, profile: String },
    #[error("this build of harness was made without ChatGPT sign-in (the `chatgpt-login` feature)")]
    SignInUnavailable,
    /// A test hook's value that is not an http(s) URL.
    #[error("{var} is `{value}`, which is not an http(s) URL")]
    BadHook { var: String, value: String },
}

/// The command that signs in to `provider` under `profile`.
pub fn login_command(provider: &str, profile: &str) -> String {
    with_profile(&format!("harness login {provider}"), profile)
}

/// How to sign in to `provider` under `profile`, as a hint: the command, or, in a build without
/// ChatGPT sign-in, that there is none.
pub fn sign_in_hint(provider: &str, profile: &str) -> String {
    if cfg!(feature = "chatgpt-login") {
        format!("sign in with `{}`", login_command(provider, profile))
    } else {
        ResolveError::SignInUnavailable.to_string()
    }
}

/// The command that stores a key for `provider` under `profile`.
pub fn auth_add_command(provider: &str, profile: &str) -> String {
    with_profile(&format!("harness auth add {provider}"), profile)
}

/// `command`, with `--profile <profile>` unless the profile is `default`.
fn with_profile(command: &str, profile: &str) -> String {
    if profile == DEFAULT_PROFILE {
        command.to_string()
    } else {
        format!("{command} --profile {profile}")
    }
}

fn store_error(provider: &str, error: CredentialError) -> ResolveError {
    ResolveError::Store {
        provider: provider.to_string(),
        message: error.to_string(),
    }
}

/// Where API keys come from: environment variables, and keys stored with `harness auth add`.
/// A closure over environment variables is a `Secrets` with nothing stored.
pub trait Secrets {
    fn env(&self, var: &str) -> Option<String>;
    /// The account profile `provider` uses.
    fn profile(&self, _provider: &str) -> Result<String, CredentialError> {
        Ok(DEFAULT_PROFILE.to_string())
    }
    /// The key stored for `provider`'s active profile.
    fn stored(&self, _provider: &str) -> Result<Option<String>, CredentialError> {
        Ok(None)
    }

    /// The credential store, for providers that sign in (`chatgpt`).
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }

    /// Where the keys and tokens a provider is given are registered as secrets, so that nothing
    /// harness writes holds them.
    fn redactor(&self) -> Option<Arc<Redactor>> {
        None
    }
}

impl<F: Fn(&str) -> Option<String>> Secrets for F {
    fn env(&self, var: &str) -> Option<String> {
        self(var)
    }
}

/// Whether `key` is a Claude subscription (OAuth) token rather than an API key, even behind a
/// byte-order mark or whitespace.
pub fn is_claude_subscription_token(key: &str) -> bool {
    key.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
        .starts_with("sk-ant-oat")
}

/// Where a provider's key came from.
enum KeySource {
    /// This environment variable.
    Env(String),
    /// The credential store, under the provider's active profile.
    Stored,
}

/// The key for provider `name`, whose key is in the environment variable `key_env`: that
/// variable, else the stored key, with where it came from. A provider without a key variable
/// takes no key, and nothing stored is looked up for it.
fn api_key(
    name: &str,
    key_env: Option<&str>,
    secrets: &impl Secrets,
) -> Result<Option<(String, KeySource)>, CredentialError> {
    // What is stored for `chatgpt` is a sign-in, which is never sent as a key.
    if name == CHATGPT {
        return Ok(None);
    }
    let Some(var) = key_env else {
        return Ok(None);
    };
    if let Some(key) = secrets.env(var).filter(|v| !v.is_empty()) {
        return Ok(Some((key, KeySource::Env(var.to_string()))));
    }
    Ok(secrets
        .stored(name)?
        .filter(|v| !v.is_empty())
        .map(|key| (key, KeySource::Stored)))
}

/// What to say when `provider` refuses its key, from `source`: which key it was, and what
/// replaces it. Never the key.
fn refused_key_hint(provider: &str, source: &KeySource, secrets: &impl Secrets) -> String {
    let profile = secrets
        .profile(provider)
        .unwrap_or_else(|_| DEFAULT_PROFILE.to_string());
    let add = auth_add_command(provider, &profile);
    match source {
        KeySource::Env(var) => format!(
            "harness sent the key in ${var}, which it uses whenever that is set: correct it, or unset {var} to use the key stored with `{add}`"
        ),
        KeySource::Stored => format!(
            "harness sent the key stored for {provider}'s profile `{profile}`: replace it with `{add}`"
        ),
    }
}

/// A provider whose refusal of its key (HTTP 401 or 403) says which key that was, and how to
/// replace it.
struct KeyHinted {
    inner: Arc<dyn Provider>,
    hint: String,
}

impl Provider for KeyHinted {
    fn stream(&self, request: ChatRequest) -> ProviderStream {
        let hint = self.hint.clone();
        Box::pin(self.inner.stream(request).map(move |item| {
            item.map_err(|error| match error {
                ProviderError::Http {
                    status: status @ (401 | 403),
                    body,
                    ..
                } => ProviderError::KeyRefused {
                    status,
                    body,
                    hint: hint.clone(),
                },
                other => other,
            })
        }))
    }
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
    // `chatgpt` is always the signed-in account: configuration cannot define it.
    if name == CHATGPT {
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
    let found = api_key(name, key_env.as_deref(), &secrets).map_err(|e| store_error(name, e))?;
    let api_key = found.as_ref().map(|(key, _)| key.clone());
    if let (None, Some(var)) = (&api_key, key_env) {
        return Err(ResolveError::MissingKey {
            provider: name.to_string(),
            profile: secrets.profile(name).map_err(|e| store_error(name, e))?,
            var,
        });
    }
    if let (Some(redactor), Some(key)) = (secrets.redactor(), &api_key) {
        redactor.add(key);
    }
    // Claude Free/Pro/Max credentials may only be used by Claude Code itself, and are never a
    // valid key for any other provider.
    if api_key.as_deref().is_some_and(is_claude_subscription_token) {
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
    let provider: Arc<dyn Provider> = match &found {
        Some((_, source)) => Arc::new(KeyHinted {
            inner: provider,
            hint: refused_key_hint(name, source, &secrets),
        }),
        None => provider,
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

/// A test hook: the value of `var`, in debug builds only, so that no release build can be
/// pointed elsewhere by its environment.
pub fn test_hook(var: &str, env: impl Fn(&str) -> Option<String>) -> Option<String> {
    if cfg!(debug_assertions) {
        env(var).filter(|value| !value.is_empty())
    } else {
        None
    }
}

/// A `chatgpt/*` model, for the account signed in under the provider's active profile.
/// `HARNESS_CHATGPT_ISSUER` and `HARNESS_CHATGPT_BASE_URL` point sign-in and requests elsewhere,
/// for tests, in debug builds only.
#[cfg(feature = "chatgpt-login")]
fn chatgpt(model_id: &str, model: &str, secrets: &impl Secrets) -> Result<Resolved, ResolveError> {
    use crate::chatgpt::{
        auth::{BASE_URL, ChatGptAuth},
        oauth::{ISSUER, OAuth},
    };
    let credentials = secrets.credentials();
    // A store that cannot be read is an error: never the default profile's account instead.
    let profile = match &credentials {
        Some(c) => c
            .active_profile(CHATGPT)
            .map_err(|e| store_error(CHATGPT, e))?,
        None => DEFAULT_PROFILE.to_string(),
    };
    let not_signed_in = || ResolveError::NotSignedIn {
        provider: CHATGPT.to_string(),
        profile: profile.clone(),
    };
    let credentials = credentials.ok_or_else(not_signed_in)?;
    let hook = |var: &str| test_hook(var, |var| secrets.env(var));
    let bad_hook = |var: &str, value: String| ResolveError::BadHook {
        var: var.to_string(),
        value,
    };
    let oauth = match hook("HARNESS_CHATGPT_ISSUER") {
        Some(issuer) => {
            OAuth::new(&issuer).map_err(|_| bad_hook("HARNESS_CHATGPT_ISSUER", issuer))?
        }
        None => OAuth::new(ISSUER).expect("the issuer is a URL"),
    };
    let base_url = hook("HARNESS_CHATGPT_BASE_URL").unwrap_or_else(|| BASE_URL.to_string());
    if !reqwest::Url::parse(&base_url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
    {
        return Err(bad_hook("HARNESS_CHATGPT_BASE_URL", base_url));
    }
    let mut auth = ChatGptAuth::load(credentials, &profile, oauth)
        .map_err(|e| store_error(CHATGPT, e))?
        .ok_or_else(not_signed_in)?;
    if let Some(redactor) = secrets.redactor() {
        auth = auth.with_redactor(redactor);
    }
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
        .filter(|(name, _)| *name != CHATGPT)
        .filter_map(|(name, cfg)| {
            let api_key = listed_key(name, cfg.api_key_env.as_deref(), &secrets)?;
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
        if let Some(Some(api_key)) = listed_key(builtin.name, builtin.key_env, &secrets) {
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
        !e.api_key
            .as_deref()
            .is_some_and(is_claude_subscription_token)
    });
    endpoints
}

/// The key a listing of `name`'s models is sent with, or `None` to leave `name` out: a store
/// that cannot be read leaves it out, with a warning.
fn listed_key(name: &str, key_env: Option<&str>, secrets: &impl Secrets) -> Option<Option<String>> {
    match api_key(name, key_env, secrets) {
        Ok(key) => Some(key.map(|(key, _)| key)),
        Err(e) => {
            if let Some(credentials) = secrets.credentials() {
                credentials.warn(format!(
                    "leaving out {name}'s models: {}",
                    store_error(name, e)
                ));
            }
            None
        }
    }
}
