//! What the interactive session asks of the CLI: the project's custom commands and `/init`,
//! what the credential store warns about, and the project's sessions.

use std::{path::PathBuf, sync::Arc};

use harness_context::commands::Commands;
use harness_core::{
    engine::PermissionEngine,
    session::{self, SessionSummary},
};
use harness_tui::app::{Host, OpenedSession, Prepared};

use crate::{
    notices::Notices,
    sessions::{self, Choice},
    setup::Setup,
    slash::{self, Message},
};

pub struct CliHost {
    pub setup: Arc<Setup>,
    pub commands: Commands,
    pub policy: Arc<PermissionEngine>,
    /// Where sandboxed commands can write, which a session's checkpoints must stay out of.
    pub writable: Vec<PathBuf>,
}

impl Host for CliHost {
    fn is_command(&self, name: &str) -> bool {
        self.commands.get(name).is_some()
    }

    fn take_warnings(&self) -> Vec<String> {
        self.setup.credentials.take_warnings()
    }

    fn prepare(&mut self, typed: &str) -> Prepared {
        let expanded =
            slash::turn_input(typed, "", Some(&self.commands), &self.setup, &*self.policy);
        let mut prepared = Prepared {
            input: expanded.input,
            notes: Vec::new(),
            warnings: Vec::new(),
        };
        // Redacted, as `harness ask` prints them.
        let redacted = |text: String| self.setup.redactor.redact(&text);
        for message in expanded.messages {
            match message {
                Message::Warning(text) => prepared.warnings.push(redacted(text)),
                Message::Note(text) => prepared.notes.push(redacted(text)),
            }
        }
        prepared
    }

    fn sessions(&self) -> Vec<SessionSummary> {
        session::list(&sessions::dir(&self.setup))
    }

    fn open_session(&self, id: Option<&str>) -> Result<OpenedSession, String> {
        let choice = match id {
            None => Choice::New,
            Some(id) => Choice::Resume(id.to_string()),
        };
        // Nothing may print while the terminal UI runs: the UI shows the warnings.
        let mut notices = Notices::quiet(self.setup.redactor.clone());
        let session = sessions::open(&self.setup, &choice, &mut notices)?;
        let checkpoints =
            sessions::checkpoints(&self.setup, &session, &self.writable, &mut notices);
        Ok(OpenedSession {
            session,
            checkpoints,
            warnings: notices.into_messages(),
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use std::path::Path;

    use harness_config::paths::Paths;
    use harness_core::{
        engine::{EngineConfig, RuleSet},
        message::Message as Said,
        permission::Mode,
        session::EntryKind,
    };

    /// The setup of a run in `workspace`, with harness's files under `home`. Credentials go to
    /// the file there, and the keychain is never opened.
    pub fn setup_in(home: &Path, workspace: &Path) -> Arc<Setup> {
        let home = home.display().to_string();
        let paths = Paths::from_env(|var| (var == "HARNESS_HOME").then(|| home.clone())).unwrap();
        let store = |var: &str| match var {
            harness_providers::credentials::STORE_ENV => Some("file".to_string()),
            harness_providers::credentials::NO_KEYCHAIN_ENV => Some("1".to_string()),
            _ => None,
        };
        Arc::new(crate::setup::load_in(workspace.to_path_buf(), paths, store).unwrap())
    }

    pub fn host(home: &Path, workspace: &Path) -> CliHost {
        let policy = Arc::new(PermissionEngine::new(EngineConfig {
            mode: Mode::Auto,
            workspace: workspace.to_path_buf(),
            read_dirs: Vec::new(),
            rules: RuleSet::default(),
            sandbox_available: false,
            writes_need_approval: false,
        }));
        CliHost {
            setup: setup_in(home, workspace),
            commands: Commands::default(),
            policy,
            writable: Vec::new(),
        }
    }

    #[test]
    fn the_host_opens_a_new_session_and_this_projects_others() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let host = host(home.path(), &workspace);
        let mut opened = host.open_session(None).unwrap();
        // The file is made with the first message.
        assert!(host.sessions().is_empty());
        opened.session.append(EntryKind::Message {
            message: Said::User {
                content: "hello".into(),
            },
            display: None,
            note: false,
            plan: None,
        });
        let id = opened.session.id().to_string();
        drop(opened);
        let listed = host.sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].first_message.as_deref(), Some("hello"));
        let resumed = host.open_session(Some(&id)).unwrap();
        assert_eq!(resumed.session.id(), id);
        assert_eq!(resumed.session.messages().len(), 1);
        let Err(why) = host.open_session(Some("nope")) else {
            panic!("an unknown session opened");
        };
        assert!(why.contains("there is no session nope"), "{why}");
    }
}
