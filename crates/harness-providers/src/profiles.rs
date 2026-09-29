//! Model profiles: per-model settings from the user's configuration, then the profiles built into
//! harness, then protocol defaults. Keys are globs over model ids, matched without regard to
//! case, and each setting is resolved on its own; within a layer the matching key with the most
//! characters other than `*` and `?` wins.

use std::collections::BTreeMap;

use globset::GlobBuilder;
use harness_config::config::ProfileSettings;
use harness_core::message::RequestOptions;

use crate::registry::LOCAL_PROVIDERS;

/// The smallest context window worth running agentic turns in, unless a profile says otherwise.
pub const DEFAULT_MIN_CONTEXT: u64 = 32_768;
/// The context window assumed when neither the server nor a profile gives one.
pub const FALLBACK_CONTEXT_WINDOW: u64 = 8_192;

/// The settings that apply to one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelProfile {
    /// `None` when no profile knows it.
    pub context_window: Option<u64>,
    pub min_context: u64,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub reasoning_effort: Option<String>,
    pub text_tool_calls: bool,
    pub local: bool,
}

impl ModelProfile {
    /// What every request to the model carries.
    pub fn request_options(&self) -> RequestOptions {
        RequestOptions {
            max_output_tokens: self.max_output_tokens,
            temperature: self.temperature,
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }
}

fn window(tokens: u64) -> ProfileSettings {
    ProfileSettings {
        context_window: Some(tokens),
        ..ProfileSettings::default()
    }
}

/// The profiles shipped in harness: common open-weight coding families, from their model cards,
/// and the hosted providers' families.
pub fn builtin_profiles() -> Vec<(&'static str, ProfileSettings)> {
    vec![
        // Qwen3-Coder: 256K tokens; Qwen recommends temperature 0.7 (with top_p 0.8, top_k 20).
        (
            "*/qwen3-coder*",
            ProfileSettings {
                temperature: Some(0.7),
                ..window(262_144)
            },
        ),
        // Qwen3: 32,768 tokens natively.
        ("*/qwen3*", window(32_768)),
        // Qwen2.5-Coder: 32,768 tokens without YaRN.
        ("*/qwen2.5-coder*", window(32_768)),
        // Devstral: 128K tokens.
        ("*/devstral*", window(131_072)),
        // gpt-oss: 128K tokens; OpenAI recommends temperature 1.0.
        (
            "*/gpt-oss*",
            ProfileSettings {
                temperature: Some(1.0),
                ..window(131_072)
            },
        ),
        // GLM-4.5: 128K tokens.
        ("*/glm-4.5*", window(131_072)),
        // DeepSeek-Coder-V2: 128K tokens.
        ("*/deepseek-coder-v2*", window(131_072)),
        // Llama 3.1: 128K tokens.
        ("*/llama3.1*", window(131_072)),
        ("*/llama-3.1*", window(131_072)),
        // Claude: 200K tokens.
        ("anthropic/*", window(200_000)),
        ("*/claude-*", window(200_000)),
        // ChatGPT's models, as Codex lists them (codex-rs/models-manager/models.json), and the
        // GPT-5 family's input limit on the API.
        ("chatgpt/*", window(272_000)),
        ("openai/gpt-5*", window(272_000)),
        ("openai/gpt-4.1*", window(1_047_576)),
        ("openai/gpt-4o*", window(128_000)),
        ("openai/o3*", window(200_000)),
        ("openai/o4-mini*", window(200_000)),
    ]
}

/// The profile of `model_id`. `local` says whether its provider is a local server (see
/// [`is_local`]); a profile may say otherwise.
pub fn resolve(
    model_id: &str,
    local: bool,
    user: &BTreeMap<String, ProfileSettings>,
) -> ModelProfile {
    let builtin = builtin_profiles();
    let layers: Vec<&ProfileSettings> =
        matching(user.iter().map(|(k, v)| (k.as_str(), v)), model_id)
            .into_iter()
            .chain(matching(builtin.iter().map(|(k, v)| (*k, v)), model_id))
            .collect();
    let local = layers.iter().find_map(|p| p.local).unwrap_or(local);
    ModelProfile {
        context_window: layers.iter().find_map(|p| p.context_window),
        min_context: layers
            .iter()
            .find_map(|p| p.min_context)
            .unwrap_or(DEFAULT_MIN_CONTEXT),
        max_output_tokens: layers.iter().find_map(|p| p.max_output_tokens),
        temperature: layers.iter().find_map(|p| p.temperature),
        reasoning_effort: layers.iter().find_map(|p| p.reasoning_effort.clone()),
        text_tool_calls: layers
            .iter()
            .find_map(|p| p.text_tool_calls)
            .unwrap_or(local),
        local,
    }
}

/// The profiles whose keys match `model_id`, the most specific first.
fn matching<'a>(
    profiles: impl Iterator<Item = (&'a str, &'a ProfileSettings)>,
    model_id: &str,
) -> Vec<&'a ProfileSettings> {
    let mut found: Vec<(usize, &ProfileSettings)> = profiles
        .filter(|(key, _)| matches(key, model_id))
        .map(|(key, profile)| {
            (
                key.chars().filter(|c| !matches!(c, '*' | '?')).count(),
                profile,
            )
        })
        .collect();
    // Stable: equally specific keys keep their order.
    found.sort_by_key(|(specificity, _)| std::cmp::Reverse(*specificity));
    found.into_iter().map(|(_, profile)| profile).collect()
}

fn matches(key: &str, model_id: &str) -> bool {
    GlobBuilder::new(key)
        .case_insensitive(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(model_id))
}

/// Whether `model_id` runs on a server of the user's own: one of the local servers harness knows
/// (wherever it runs), or a server on the loopback interface.
pub fn is_local(model_id: &str, base_url: &str) -> bool {
    let provider = model_id.split('/').next().unwrap_or_default();
    if LOCAL_PROVIDERS.contains(&provider) {
        return true;
    }
    let Some(host) = reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(String::from))
    else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
