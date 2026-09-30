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
