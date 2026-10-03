//! The language servers of a session: the settings that say which run and how long to wait, from
//! the configuration and the environment.

use std::{sync::Arc, time::Duration};

use harness_config::trust::TrustStore;
use harness_core::agent::Approver;
use harness_lsp::{AskThroughApprover, LspDiagnostics, Manager, ServerSetting, Settings};

use crate::setup::Setup;

/// The first request to a server waits this long while it indexes (or `lsp.wait_ms`, if longer).
const FIRST_WAIT: Duration = Duration::from_secs(10);

/// How long a server gets to answer `initialize`.
const INIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The settings servers run with in `setup`'s workspace.
pub fn settings(setup: &Setup) -> Settings {
    let lsp = &setup.config.lsp;
    Settings {
        enabled: lsp.enabled,
        wait: Duration::from_millis(lsp.wait_ms),
        first_wait: FIRST_WAIT,
        servers: lsp
            .servers
            .iter()
            .map(|(language, server)| {
                (
                    language.clone(),
                    ServerSetting {
                        command: server.command.clone(),
                        enabled: server.enabled,
                    },
                )
            })
            .collect(),
        trusted: setup.config.trusted,
        allowed: setup.config.lsp_servers_allowed,
        path: (setup.env)("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default(),
        init_timeout: INIT_TIMEOUT,
    }
}

/// The diagnostics for a session in `setup`'s workspace. With an `approver` (an interactive
/// session), the first edit of a file that has a server, in a workspace not yet trusted for
/// servers, asks once whether they may start, and the answer is stored with the workspace's trust
/// record. Without one, servers start only where the workspace is trusted or the stored answer is
/// yes.
pub fn diagnostics(setup: &Setup, approver: Option<Arc<dyn Approver>>) -> Arc<LspDiagnostics> {
    let mut manager = Manager::new(settings(setup), setup.workspace.clone());
    if let Some(approver) = approver {
        let workspace = setup.workspace.clone();
        let data_dir = setup.paths.data_dir.clone();
        let stored = workspace.clone();
        manager = manager.with_consent(Arc::new(AskThroughApprover::new(
            approver,
            workspace,
            // A trust file that cannot be written leaves the answer for this session only.
            move |yes| {
                if let Ok(mut store) = TrustStore::load(&data_dir) {
                    let _ = store.set_servers_answer(&stored, yes);
                }
            },
        )));
    }
    Arc::new(LspDiagnostics::new(manager, setup.workspace.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use harness_config::config::{Config, LspConfig, LspServer};

    fn setup(config: Config, path: Option<&str>) -> Arc<Setup> {
        let home = tempfile::tempdir().unwrap();
        let ws = home.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let vars: Vec<(&str, &str)> = path.map(|p| ("PATH", p)).into_iter().collect();
        let setup = crate::host::tests::setup_with(home.path(), &ws, &vars);
        let mut owned = Arc::try_unwrap(setup).ok().unwrap();
        owned.config = config;
        Arc::new(owned)
    }

    #[test]
    fn the_configuration_becomes_the_managers_settings() {
        let config = Config {
            trusted: true,
            lsp: LspConfig {
                enabled: true,
                wait_ms: 750,
                servers: [(
                    "python".to_string(),
                    LspServer {
                        command: Some("pylsp".into()),
                        enabled: true,
                    },
                )]
                .into(),
            },
            ..Config::default()
        };
        let s = settings(&setup(config, Some("/usr/bin:/opt/bin")));
        assert!(s.enabled && s.trusted);
        assert_eq!(s.wait, Duration::from_millis(750));
        assert_eq!(s.first_wait, Duration::from_secs(10));
        assert_eq!(
            s.path,
            [PathBuf::from("/usr/bin"), PathBuf::from("/opt/bin")]
        );
        assert_eq!(s.servers["python"].command.as_deref(), Some("pylsp"));
    }

    // Spec: servers start only in trusted workspaces.
    #[test]
    fn an_untrusted_workspace_is_passed_on_as_untrusted() {
        let s = settings(&setup(Config::default(), None));
        assert!(!s.trusted);
        assert!(s.path.is_empty());
    }

    // Ruling P3: the stored answer reaches the manager, so a headless run in a workspace the user
    // said yes to starts servers, and one never asked does not.
    #[test]
    fn the_stored_answer_becomes_the_managers_setting() {
        for answer in [None, Some(true), Some(false)] {
            let config = Config {
                lsp_servers_allowed: answer,
                ..Config::default()
            };
            let s = settings(&setup(config, None));
            assert!(!s.trusted);
            assert_eq!(s.allowed, answer);
        }
    }
}
