//! Making the models of roles ready: the CLI's side of `harness_core::role::ModelResolver`. A
//! model id becomes a provider and the model's whole profile, once, when a turn first needs it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use futures::future::BoxFuture;
use harness_core::{role::ModelResolver, turn::TurnModel};
use harness_providers::{registry, window::Running};
use tokio_util::sync::CancellationToken;

use crate::setup::Setup;

/// Resolves model ids with the session's configuration and credentials, and keeps what it
/// resolved: a role's model is made ready once per session.
pub struct CliResolver {
    setup: Arc<Setup>,
    cache: Arc<Mutex<HashMap<String, TurnModel>>>,
}

impl CliResolver {
    pub fn new(setup: Arc<Setup>) -> Arc<CliResolver> {
        Arc::new(CliResolver {
            setup,
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}

impl ModelResolver for CliResolver {
    fn chain(&self, model_id: &str) -> Vec<String> {
        harness_providers::profiles::best_match(
            self.setup
                .config
                .fallback
                .iter()
                .map(|(glob, chain)| (glob.as_str(), chain)),
            model_id,
        )
        .cloned()
        .unwrap_or_default()
    }

    fn resolve(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<TurnModel, String>> {
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        let (setup, cache, id) = (self.setup.clone(), self.cache.clone(), id.to_string());
        Box::pin(async move {
            if let Some(model) = cached {
                return Ok(model);
            }
            // Looking up the key can wait on the keychain (an unlock prompt, say), so it runs on
            // a thread meant for blocking, and Esc stops the wait.
            let resolving = {
                let (setup, id) = (setup.clone(), id.clone());
                tokio::task::spawn_blocking(move || {
                    registry::resolve(&id, &setup.config.providers, setup.keys())
                        .map_err(|e| e.to_string())
                })
            };
            let resolved = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err("stopped".to_string()),
                resolved = resolving => resolved.map_err(|e| format!("resolving it failed: {e}"))??,
            };
            let (running, server) = crate::start::running_window(&setup, &resolved, &cancel)
                .await
                .ok_or("stopped")?;
            let model = crate::slash::turn_model(resolved, &setup.config.profiles, running);
            // A local server that did not answer is asked again next time.
            if server.is_none() || running != Running::Unknown {
                cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, model.clone());
            }
            Ok(model)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::host::tests::setup_with;

    /// A setup whose config is `config`, in a workspace under a temporary directory.
    fn resolver(config: &str, vars: &[(&str, &str)]) -> (tempfile::TempDir, Arc<CliResolver>) {
        let dir = tempfile::tempdir().unwrap();
        let (home, workspace) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(home.join("config/config.toml"), config).unwrap();
        let setup = setup_with(&home, &workspace.canonicalize().unwrap(), vars);
        (dir, CliResolver::new(setup))
    }

    const LOCAL: &str = "[providers.mine]\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9/v1\"\n[profiles.\"mine/x\"]\ncontext_window = 65536\nmax_output_tokens = 123\ntemperature = 0.2\n";

    #[tokio::test]
    async fn a_hosted_model_gets_its_providers_profile() {
        let (_dir, resolver) = resolver("", &[("OPENAI_API_KEY", "sk-test")]);
        let model = resolver
            .resolve("openai/gpt-5", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (model.id.as_str(), model.name.as_str()),
            ("openai/gpt-5", "gpt-5")
        );
        assert_eq!(model.context_window, Some(272_000));
        assert!(!model.local);
        // Its edit tools and the prompt's edit section go with it.
        assert!(model.tools.is_some() && model.edit_section.is_some());
    }

    #[tokio::test]
    async fn a_local_model_gets_the_window_and_options_of_its_profile() {
        let (_dir, resolver) = resolver(LOCAL, &[]);
        let model = resolver
            .resolve("mine/x", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(model.context_window, Some(65_536));
        assert!(model.local && model.text_tool_calls);
        let options = model.request.unwrap();
        assert_eq!(options.max_output_tokens, Some(123));
        assert_eq!(options.temperature, Some(0.2));
        assert!(options.local);
    }

    #[tokio::test]
    async fn a_model_that_cannot_be_used_says_why() {
        let (_dir, resolver) = resolver("", &[]);
        // No key for it.
        let why = resolver
            .resolve("openai/gpt-5", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(why.contains("openai"), "{why}");
        let why = resolver
            .resolve("nowhere/x", CancellationToken::new())
            .await
            .unwrap_err();
        assert!(why.contains("nowhere"), "{why}");
    }

    #[tokio::test]
    async fn a_model_is_made_ready_once() {
        let (_dir, resolver) = resolver(LOCAL, &[]);
        let first = resolver
            .resolve("mine/x", CancellationToken::new())
            .await
            .unwrap();
        let second = resolver
            .resolve("mine/x", CancellationToken::new())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first.provider, &second.provider));
    }

    // The chain of a failed model is the one whose glob is the most specific, case aside.
    #[test]
    fn the_chain_comes_from_the_most_specific_glob() {
        let (_dir, resolver) = resolver(
            &format!(
                "{LOCAL}[fallback]\n\"chatgpt/*\" = [\"openai/a\"]\n\"chatgpt/gpt-5*\" = [\"openai/b\", \"openai/c\"]\n"
            ),
            &[],
        );
        assert_eq!(
            resolver.chain("chatgpt/gpt-5-codex"),
            ["openai/b", "openai/c"]
        );
        assert_eq!(resolver.chain("ChatGPT/o3"), ["openai/a"]);
        assert!(resolver.chain("openai/gpt-5").is_empty());
    }

    #[tokio::test]
    async fn stopping_the_wait_stops_the_resolution() {
        let (_dir, resolver) = resolver(LOCAL, &[]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let why = resolver.resolve("mine/x", cancel).await.unwrap_err();
        assert_eq!(why, "stopped");
    }
}
