use std::time::Duration;

use serde_json::Value;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub provider: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

/// Lists models from OpenAI-compatible `/models` endpoints concurrently. Unreachable, slow, or failing
/// endpoints are skipped. Results keep endpoint order; models within an endpoint are sorted by name.
pub async fn list_models(endpoints: &[Endpoint], timeout: Duration) -> Vec<DiscoveredModel> {
    let Ok(client) = reqwest::Client::builder().timeout(timeout).build() else {
        return Vec::new();
    };
    let probes = endpoints.iter().map(|endpoint| {
        let client = client.clone();
        async move {
            let mut request = client.get(format!(
                "{}/models",
                endpoint.base_url.trim_end_matches('/')
            ));
            if let Some(key) = &endpoint.api_key {
                request = request.bearer_auth(key);
            }
            let Ok(response) = request.send().await.and_then(|r| r.error_for_status()) else {
                return Vec::new();
            };
            let Ok(body) = response.json::<Value>().await else {
                return Vec::new();
            };
            let mut names: Vec<String> = body["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["id"].as_str().map(String::from))
                .collect();
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
