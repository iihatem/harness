use harness_providers::{
    discovery::{self, DiscoveredModel, LOCAL_PROBE_TIMEOUT, REMOTE_PROBE_TIMEOUT},
    registry,
};

use crate::{
    setup::{self, Setup},
    term::terminal_safe,
};

/// Models from local servers and configured providers, local first.
pub async fn available(setup: &Setup) -> Vec<DiscoveredModel> {
    let local = registry::local_endpoints(&setup.config.providers);
    let configured = registry::configured_endpoints(&setup.config.providers, setup.keys());
    let (mut found, remote) = tokio::join!(
        discovery::list_models(&local, LOCAL_PROBE_TIMEOUT),
        discovery::list_models(&configured, REMOTE_PROBE_TIMEOUT)
    );
    found.extend(remote);
    found
}

/// The ids of the models to choose from: those found (see [`available`]), then ChatGPT's own
/// when an account is signed in (ChatGPT has no listing to ask, so the built-in list is
/// offered).
pub async fn choices(setup: &Setup) -> Vec<String> {
    let mut ids: Vec<String> = available(setup)
        .await
        .into_iter()
        .map(|model| model.id())
        .collect();
    if signed_in_to_chatgpt(setup).await {
        ids.extend(
            registry::CHATGPT_MODELS
                .iter()
                .map(|model| format!("{}/{model}", registry::CHATGPT)),
        );
    }
    ids
}

/// Whether a ChatGPT account is signed in. The credential store may wait on the keychain, which
/// is not done on the runtime's threads.
#[cfg(feature = "chatgpt-login")]
async fn signed_in_to_chatgpt(setup: &Setup) -> bool {
    let credentials = setup.credentials.clone();
    tokio::task::spawn_blocking(move || {
        credentials
            .active(registry::CHATGPT)
            .is_ok_and(|stored| stored.is_some())
    })
    .await
    .unwrap_or(false)
}

#[cfg(not(feature = "chatgpt-login"))]
async fn signed_in_to_chatgpt(_setup: &Setup) -> bool {
    false
}

pub async fn run() -> u8 {
    let setup = match setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    let found = available(&setup).await;
    setup.print_credential_warnings();
    if found.is_empty() {
        eprintln!(
            "No models found. Start Ollama, LM Studio, or llama.cpp, or configure a provider in {}.",
            setup.paths.global_config_file().display()
        );
    }
    for model in found {
        println!("{}", terminal_safe(&model.id()));
    }
    0
}
