use std::time::Duration;

use harness_config::config::Protocol;
use serde_json::Value;

use crate::anthropic_messages::API_VERSION;

/// Each local-server probe gives up after this long, so absent servers never delay startup.
pub const LOCAL_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// Remote `/models` listings get longer.
pub const REMOTE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub provider: String,
    pub name: String,
}

impl DiscoveredModel {
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider, self.name)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
    /// How the key is sent: Anthropic's own headers, or a bearer token.
    pub protocol: Protocol,
}

/// Leaves the key out, so that it never reaches a log or an error message.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("protocol", &self.protocol)
            .finish()
    }
}

/// Lists models from `/models` endpoints (OpenAI-compatible, or Anthropic's, which answers in the
/// same shape) concurrently. Unreachable, slow, or failing
/// endpoints are skipped. Results keep endpoint order; models within an endpoint are sorted by name.
/// Anthropic's listing is read page by page, up to [`MAX_PAGES`]; OpenAI's is narrowed to the
/// models that can answer a turn.
pub async fn list_models(endpoints: &[Endpoint], timeout: Duration) -> Vec<DiscoveredModel> {
    let Ok(client) = crate::http::client().timeout(timeout).build() else {
        return Vec::new();
    };
    let probes = endpoints.iter().map(|endpoint| {
        let client = client.clone();
        async move {
            let mut names = model_names(&client, endpoint).await.unwrap_or_default();
            if endpoint.provider == "openai" {
                names.retain(|name| answers_turns(name));
            }
            names.sort();
            names
                .into_iter()
                .map(|name| DiscoveredModel {
                    provider: endpoint.provider.clone(),
                    name,
                })
                .collect::<Vec<_>>()
        }
    });
    futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Pages of Anthropic's listing read at most: a thousand models each.
pub const MAX_PAGES: usize = 10;

/// The model names `endpoint` lists, or `None` when it cannot be asked or its first page does
/// not come.
async fn model_names(client: &reqwest::Client, endpoint: &Endpoint) -> Option<Vec<String>> {
    let url = format!("{}/models", endpoint.base_url.trim_end_matches('/'));
    let anthropic = endpoint.protocol == Protocol::AnthropicMessages;
    let mut names = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..if anthropic { MAX_PAGES } else { 1 } {
        let mut url = reqwest::Url::parse(&url).ok()?;
        let mut request = if anthropic {
            // Twenty models a page unless asked for more.
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(after) = &after {
                url.query_pairs_mut().append_pair("after_id", after);
            }
            let mut request = client.get(url).header("anthropic-version", API_VERSION);
            if let Some(key) = &endpoint.api_key {
                request = request.header("x-api-key", key);
            }
            request
        } else {
            client.get(url)
        };
        if let (false, Some(key)) = (anthropic, &endpoint.api_key) {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.and_then(|r| r.error_for_status());
        let body = match response {
            Ok(response) => response.json::<Value>().await.ok(),
            Err(_) => None,
        };
        // A later page that fails leaves the ones read so far.
        let Some(body) = body else {
            return (after.is_some()).then_some(names);
        };
        names.extend(
            body["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().map(String::from)),
        );
        after = body["last_id"]
            .as_str()
            .filter(|_| body["has_more"] == true)
            .map(String::from);
        if after.is_none() {
            break;
        }
    }
    Some(names)
}

/// Whether OpenAI's model `name` can answer a turn: the GPT, o-series, ChatGPT and Codex
/// families, without their embedding, audio, realtime, speech, image, search (deep research
/// included), moderation and completion-only models.
fn answers_turns(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let family = ["gpt-", "o1", "o3", "o4", "chatgpt-", "codex-"]
        .iter()
        .any(|prefix| name.starts_with(prefix));
    let other = [
        "embedding",
        "audio",
        "realtime",
        "transcribe",
        "tts",
        "image",
        // Search models, and the deep-research ones, which need a web-search tool harness does
        // not send.
        "search",
        "moderation",
        "instruct",
    ]
    .iter()
    .any(|kind| name.contains(kind));
    family && !other
}
