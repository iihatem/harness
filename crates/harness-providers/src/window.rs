//! The context window a local server really runs a model with, which can be smaller than the
//! model's own: llama.cpp's `/props`, Ollama's running models, LM Studio's loaded instances. A
//! server that does not answer in time reports nothing. The window harness uses is the smaller of
//! that and the model's profile.

use std::{collections::BTreeMap, time::Duration};

use harness_config::config::{Protocol, ProviderConfig};
use serde_json::{Value, json};

use crate::profiles::{FALLBACK_CONTEXT_WINDOW, ModelProfile};

/// How long each question to a server may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long Ollama may take to load a model it is asked about.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// A local server that reports the context it runs models with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    Ollama,
    LlamaCpp,
    LmStudio,
}

impl Server {
    /// The server behind provider `provider`: one of the built-in local servers, by name, also
    /// when the user moved it to another address, as long as it speaks the Chat Completions
    /// protocol.
    pub fn of(provider: &str, providers: &BTreeMap<String, ProviderConfig>) -> Option<Server> {
        if providers
            .get(provider)
            .is_some_and(|cfg| cfg.protocol != Protocol::OpenaiChat)
        {
            return None;
        }
        match provider {
            "ollama" => Some(Server::Ollama),
            "llamacpp" => Some(Server::LlamaCpp),
            "lmstudio" => Some(Server::LmStudio),
            _ => None,
        }
    }

    /// How to run the model with at least `tokens` tokens of context on this server.
    fn remedy(self, tokens: u64) -> String {
        match self {
            Server::Ollama => format!(
                "restart Ollama with a larger context, e.g. `OLLAMA_CONTEXT_LENGTH={tokens} ollama serve`"
            ),
            Server::LlamaCpp => format!(
                "start llama-server with a larger context, e.g. `llama-server -c {tokens}` (with --parallel, each slot gets a share)"
            ),
            Server::LmStudio => {
                format!("load the model in LM Studio with a context length of {tokens} or more")
            }
        }
    }
}

/// The context `server`, whose Chat Completions endpoint is `base_url`, runs `model` with, if it
/// says. Ollama is asked to load a model it has not loaded yet, which the first request would do
/// anyway, taking up to `load`.
pub async fn running_context(
    server: Server,
    base_url: &str,
    model: &str,
    probe: Duration,
    load: Duration,
) -> Option<u64> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let client = reqwest::Client::new();
    match server {
        Server::Ollama => {
            if let Some(tokens) = ollama_running(&client, root, model, probe).await {
                return Some(tokens);
            }
            // An empty prompt only loads the model.
            client
                .post(format!("{root}/api/generate"))
                .json(&json!({"model": model}))
                .timeout(load)
                .send()
                .await
                .ok()?;
            ollama_running(&client, root, model, probe).await
        }
        Server::LlamaCpp => {
            let props = get(
                &client,
                &format!("{root}/props"),
                &[("model", model)],
                probe,
            )
            .await?;
            props["default_generation_settings"]["n_ctx"].as_u64()
        }
        Server::LmStudio => {
            let listing = get(&client, &format!("{root}/api/v1/models"), &[], probe).await?;
            listing["models"]
                .as_array()?
                .iter()
                .flat_map(|m| {
                    let key = m["key"].as_str() == Some(model);
                    m["loaded_instances"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(move |i| key || i["id"].as_str() == Some(model))
                })
                .filter_map(|instance| instance["config"]["context_length"].as_u64())
                .min()
        }
    }
}

/// The context of `model` among Ollama's running models. A name without a tag is `:latest`.
async fn ollama_running(
    client: &reqwest::Client,
    root: &str,
    model: &str,
    probe: Duration,
) -> Option<u64> {
    let tagged = if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    };
    let running = get(client, &format!("{root}/api/ps"), &[], probe).await?;
    running["models"].as_array()?.iter().find(|m| {
        [&m["name"], &m["model"]]
            .iter()
            .any(|name| name.as_str().is_some_and(|n| n == model || n == tagged))
    })?["context_length"]
        .as_u64()
}

async fn get(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
    timeout: Duration,
) -> Option<Value> {
    let mut url = reqwest::Url::parse(url).ok()?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    response.json().await.ok()
}

/// The window harness uses, and what to tell the user about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub tokens: u64,
    pub warnings: Vec<String>,
}

/// The window of model `id`: the smaller of what its server runs it with (`running`) and its
/// profile's, or the fallback when neither is known. A window below the profile's minimum is
/// warned about, with the fix for its server.
pub fn effective_window(
    id: &str,
    profile: &ModelProfile,
    running: Option<u64>,
    server: Option<Server>,
) -> Window {
    let mut warnings = Vec::new();
    let tokens = match (running, profile.context_window) {
        (Some(running), Some(own)) => running.min(own),
        (Some(running), None) => running,
        (None, Some(own)) => own,
        (None, None) => {
            warnings.push(format!(
                "the context window of {id} is unknown; assuming {FALLBACK_CONTEXT_WINDOW} tokens. Set it with `context_window` under [profiles.\"{id}\"] in config.toml"
            ));
            return Window {
                tokens: FALLBACK_CONTEXT_WINDOW,
                warnings,
            };
        }
    };
    if tokens < profile.min_context {
        let fix = match (running, server) {
            (Some(running), Some(server)) if running == tokens => {
                server.remedy(profile.min_context)
            }
            _ => "raise `context_window` in its profile if the model takes more, or choose a model with a larger window".to_string(),
        };
        warnings.push(format!(
            "{id} runs with a {tokens}-token context window, below the {} tokens agentic work needs, so long tasks will be compacted often; {fix}",
            profile.min_context
        ));
    }
    Window { tokens, warnings }
}
