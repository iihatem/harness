//! What the interactive session asks of the CLI: the project's custom commands and `/init`,
//! what the credential store warns about, and the project's sessions.

use std::{path::PathBuf, sync::Arc};

use futures::future::BoxFuture;
use harness_context::commands::Commands;
use harness_core::{
    agent::SessionModel,
    engine::PermissionEngine,
    session::{self, SessionSummary},
};
use harness_providers::registry;
use harness_tui::app::{Host, ModelSwitch, OpenedSession, Prepared};
use tokio_util::sync::CancellationToken;

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

    fn models(&self) -> BoxFuture<'static, Vec<String>> {
        let setup = self.setup.clone();
        Box::pin(async move {
            crate::models::available(&setup)
                .await
                .into_iter()
                .map(|model| model.id())
                .collect()
        })
    }

    fn switch_model(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<ModelSwitch, String>> {
        let setup = self.setup.clone();
        let id = id.to_string();
        Box::pin(async move {
            let resolved = registry::resolve(&id, &setup.config.providers, setup.keys())
                .map_err(|e| e.to_string())?;
            let model = crate::start::model_setup(&setup, &resolved, &cancel)
                .await
                .ok_or("stopped")?;
            Ok(ModelSwitch {
                model: SessionModel {
                    provider: resolved.provider,
                    id: resolved.id,
                    name: resolved.model,
                    context_window: model.context_window,
                    request: model.request,
                    text_tool_calls: model.text_tool_calls,
                },
                window_note: model.window_note.into(),
                warnings: model.warnings,
            })
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
    use harness_providers::registry;
    use harness_tui::{
        app::Options, approval::ChannelApprover, inline::InlineTerminal, style::Theme, ui::Ui,
    };
    use ratatui::{
        backend::TestBackend,
        crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
    };
    use tokio_util::sync::CancellationToken;

    use crate::start::{self, Request, Started};

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

    /// A Chat Completions stream of `chunks`.
    fn chat_stream(chunks: &[serde_json::Value]) -> wiremock::ResponseTemplate {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    /// An Anthropic Messages stream answering `text`.
    fn anthropic_stream(text: &str) -> wiremock::ResponseTemplate {
        use serde_json::json;
        let events = [
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "content": [], "model": "opus", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ];
        let body: String = events
            .iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect();
        wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
    }

    fn press(ui: &mut Ui<TestBackend>, code: KeyCode) {
        ui.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .unwrap();
    }

    fn send(ui: &mut Ui<TestBackend>, text: &str) {
        for c in text.chars() {
            press(ui, KeyCode::Char(c));
        }
        press(ui, KeyCode::Enter);
    }

    async fn settle(ui: &mut Ui<TestBackend>) {
        tokio::time::timeout(std::time::Duration::from_secs(20), ui.settle())
            .await
            .expect("the work ends")
            .unwrap();
    }

    // M1's done criterion, with mock servers: a conversation held on a Chat Completions model
    // continues on an Anthropic model after `/model`. The tool call's id and the reply that was
    // only whitespace, which the Messages API would reject, reach it in a form it takes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_conversation_on_a_chat_model_continues_on_anthropic() {
        use serde_json::{Value, json};
        use wiremock::{
            Mock, MockServer,
            matchers::{method, path},
        };

        let chat = MockServer::start().await;
        let claude = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(chat_stream(&[
                json!({"choices": [{"index": 0, "delta": {"content": "\n\n"}, "finish_reason": null}]}),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "functions.read:0", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"notes.txt\"}"}}]}, "finish_reason": "tool_calls"}]}),
            ]))
            .up_to_n_times(1)
            .mount(&chat)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(chat_stream(&[
                json!({"choices": [{"index": 0, "delta": {"content": "The notes say hi."}, "finish_reason": "stop"}]}),
            ]))
            .mount(&chat)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_stream("Done on Claude."))
            .mount(&claude)
            .await;
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join("notes.txt"), "hi\n").unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "[providers.chat]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[providers.claude]\nprotocol = \"anthropic-messages\"\nbase_url = \"{}/v1\"\n[profiles.\"chat/*\"]\ncontext_window = 32768\n[profiles.\"claude/*\"]\ncontext_window = 200000\n",
                chat.uri(),
                claude.uri()
            ),
        )
        .unwrap();
        let setup = setup_in(home.path(), &workspace);
        let mut notices = Notices::quiet(setup.redactor.clone());
        let session = sessions::open(&setup, &Choice::New, &mut notices).unwrap();
        let session_path = session.path().unwrap().to_path_buf();
        let resolved =
            registry::resolve("chat/small", &setup.config.providers, setup.keys()).unwrap();
        let (approver, approvals) = ChannelApprover::new();
        let Some(Started {
            agent,
            sandbox_session,
            policy,
            window_note,
            writable,
            ..
        }) = start::start(
            Request {
                setup: &setup,
                mode: Mode::Auto,
                model: resolved,
                session,
                approver,
                interactive: true,
                run_id: start::run_id(),
                cancel: CancellationToken::new(),
            },
            &mut notices,
        )
        .await
        else {
            panic!("the start was cancelled");
        };
        let host = CliHost {
            setup: setup.clone(),
            commands: Commands::default(),
            policy,
            writable,
        };
        let options = Options {
            theme: Theme::monochrome(),
            model: "chat/small".into(),
            mode: Mode::Auto,
            commands: Vec::new(),
            workspace: workspace.clone(),
            history: Vec::new(),
            instruction_files: Vec::new(),
            window_note: Some(window_note),
            default_mode: Mode::Auto,
            text_editor: None,
            notifier: None,
        };
        let term = InlineTerminal::new(TestBackend::new(100, 30), 0).unwrap();
        let mut ui = Ui::start(agent, Box::new(host), term, options, approvals)
            .with_redactor(setup.redactor.clone());
        ui.draw().unwrap();
        send(&mut ui, "read the notes");
        settle(&mut ui).await;
        send(&mut ui, "/model claude/opus");
        settle(&mut ui).await;
        send(&mut ui, "and now?");
        settle(&mut ui).await;
        ui.finish().await.unwrap();
        sandbox_session.end();

        let requests = claude.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let text = body.to_string();
        assert_eq!(body["model"], "opus", "{text}");
        assert!(!text.contains("functions.read:0"), "{text}");
        for said in ["read the notes", "The notes say hi.", "and now?"] {
            assert!(text.contains(said), "{said}: {text}");
        }
        let blocks: Vec<&Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .collect();
        let uses: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        let results: Vec<&str> = blocks
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| b["tool_use_id"].as_str().unwrap())
            .collect();
        assert_eq!(uses.len(), 1, "{text}");
        assert_eq!(uses, results);
        assert!(
            uses[0]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{}",
            uses[0]
        );
        assert!(
            !blocks
                .iter()
                .any(|b| b["type"] == "text" && b["text"].as_str().unwrap_or("").trim().is_empty()),
            "{text}"
        );
        // Each reply carries the model that wrote it.
        let (saved, _) = harness_core::session::Session::open(&session_path).unwrap();
        let models: Vec<String> = saved
            .messages()
            .into_iter()
            .filter_map(|(_, m)| match m {
                Said::Assistant { model, .. } => Some(model),
                _ => None,
            })
            .collect();
        assert_eq!(models, ["chat/small", "chat/small", "claude/opus"]);
    }
}
