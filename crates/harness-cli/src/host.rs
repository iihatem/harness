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

    fn open_session(
        &self,
        id: Option<&str>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OpenedSession, String>> {
        let choice = match id {
            None => Choice::New,
            Some(id) => Choice::Resume(id.to_string()),
        };
        let setup = self.setup.clone();
        let writable = self.writable.clone();
        // Reading the file and opening the checkpoints (which runs `git`) block, so they run on
        // a thread meant for that.
        let opening = tokio::task::spawn_blocking(move || {
            // Nothing may print while the terminal UI runs: the UI shows the warnings.
            let mut notices = Notices::quiet(setup.redactor.clone());
            let session =
                sessions::open_listing(&setup, &choice, &mut notices, "`/resume` lists them")?;
            let checkpoints = sessions::checkpoints(&setup, &session, &writable, &mut notices);
            Ok(OpenedSession {
                session,
                checkpoints,
                warnings: notices.into_messages(),
            })
        });
        Box::pin(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err("stopped".to_string()),
                opened = opening => opened.unwrap_or_else(|e| Err(format!("opening it failed: {e}"))),
            }
        })
    }

    fn models(&self) -> BoxFuture<'static, Vec<String>> {
        let setup = self.setup.clone();
        Box::pin(async move { crate::models::choices(&setup).await })
    }

    fn login(
        &self,
        provider: &str,
        device: bool,
        notes: tokio::sync::mpsc::UnboundedSender<String>,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String, String>> {
        let setup = self.setup.clone();
        let provider = provider.to_string();
        Box::pin(async move {
            let say = move |text: String| {
                let _ = notes.send(text);
            };
            crate::login::in_session(&setup, &provider, device, &say, cancel).await
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
        setup_with(home, workspace, &[])
    }

    /// [`setup_in`], with the environment variables `vars` and no others, for the credential
    /// store and the test hooks.
    pub fn setup_with(home: &Path, workspace: &Path, vars: &[(&str, &str)]) -> Arc<Setup> {
        let home = home.display().to_string();
        let paths = Paths::from_env(|var| (var == "HARNESS_HOME").then(|| home.clone())).unwrap();
        let mut env: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.insert(
            harness_providers::credentials::STORE_ENV.into(),
            "file".into(),
        );
        env.insert(
            harness_providers::credentials::NO_KEYCHAIN_ENV.into(),
            "1".into(),
        );
        let env: crate::setup::Env = Arc::new(move |var: &str| env.get(var).cloned());
        Arc::new(crate::setup::load_in(workspace.to_path_buf(), paths, env).unwrap())
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

    /// What the host opens, waited for.
    fn open(host: &CliHost, id: Option<&str>) -> Result<OpenedSession, String> {
        futures::executor::block_on(host.open_session(id, CancellationToken::new()))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_host_opens_a_new_session_and_this_projects_others() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let host = host(home.path(), &workspace);
        let mut opened = open(&host, None).unwrap();
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
        let resumed = open(&host, Some(&id)).unwrap();
        assert_eq!(resumed.session.id(), id);
        assert_eq!(resumed.session.messages().len(), 1);
        let Err(why) = open(&host, Some("nope")) else {
            panic!("an unknown session opened");
        };
        assert!(why.contains("there is no session nope"), "{why}");
        // Review B M6: in a session, `/resume` lists the sessions, not `harness --resume`.
        assert!(why.contains("/resume"), "{why}");
        assert!(!why.contains("harness --resume"), "{why}");
    }

    // Review B I1: with ChatGPT signed in, the model list (the picker's) has ChatGPT's models,
    // from the built-in list; without it, none.
    #[cfg(feature = "chatgpt-login")]
    #[tokio::test]
    async fn a_signed_in_chatgpt_account_offers_its_models() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let host = host(home.path(), &workspace);
        let before = host.models().await;
        assert!(
            !before.iter().any(|id| id.starts_with("chatgpt/")),
            "{before:?}"
        );
        let profile = host.setup.credentials.active_profile("chatgpt").unwrap();
        host.setup
            .credentials
            .set("chatgpt", &profile, "{}")
            .unwrap();
        let after = host.models().await;
        let chatgpt: Vec<&String> = after
            .iter()
            .filter(|id| id.starts_with("chatgpt/"))
            .collect();
        assert!(!chatgpt.is_empty(), "{after:?}");
        for model in registry::CHATGPT_MODELS {
            assert!(
                after.contains(&format!("chatgpt/{model}")),
                "{model}: {after:?}"
            );
        }
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

    // M1's done criterion, with mock servers: ChatGPT sign-in inside the session, then a
    // mid-session `/model` switch to a ChatGPT model, which answers with the signed-in account.
    #[cfg(feature = "chatgpt-login")]
    #[tokio::test(flavor = "multi_thread")]
    async fn signing_in_mid_session_lets_model_switch_to_chatgpt() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use serde_json::{Value, json};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{header, method, path},
        };

        let jwt = |claims: Value| {
            let part = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
            format!(
                "{}.{}.{}",
                part(&json!({"alg": "none"})),
                part(&claims),
                URL_SAFE_NO_PAD.encode(b"sig")
            )
        };
        let access = jwt(json!({"exp": 4_102_444_800u64}));
        let chat = MockServer::start().await;
        let openai = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(chat_stream(&[
                json!({"choices": [{"index": 0, "delta": {"content": "Hello from the local model."}, "finish_reason": "stop"}]}),
            ]))
            .mount(&chat)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/usercode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_auth_id": "device-auth-1",
                "user_code": "ABCD-1234",
                "interval": "0"
            })))
            .mount(&openai)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/accounts/deviceauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "authorization_code": "code-1",
                "code_challenge": "challenge",
                "code_verifier": "verifier"
            })))
            .mount(&openai)
            .await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id_token": jwt(json!({
                    "email": "dev@example.com",
                    "https://api.openai.com/auth": {"chatgpt_account_id": "acct-123"}
                })),
                "access_token": access,
                "refresh_token": "rt-canary-0123456789"
            })))
            .mount(&openai)
            .await;
        let answer = [
            json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hi from ChatGPT."}),
            json!({"type": "response.completed", "response": {"status": "completed"}}),
        ]
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>();
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .and(header("authorization", format!("Bearer {access}").as_str()))
            .and(header("chatgpt-account-id", "acct-123"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
            .mount(&openai)
            .await;

        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/config.toml"),
            format!(
                "[providers.chat]\nprotocol = \"openai-chat\"\nbase_url = \"{}/v1\"\n[profiles.\"chat/*\"]\ncontext_window = 32768\n",
                chat.uri()
            ),
        )
        .unwrap();
        let base_url = format!("{}/backend-api/codex", openai.uri());
        let setup = setup_with(
            home.path(),
            &workspace,
            &[
                ("HARNESS_CHATGPT_ISSUER", &openai.uri()),
                ("HARNESS_CHATGPT_BASE_URL", &base_url),
            ],
        );
        let mut notices = Notices::quiet(setup.redactor.clone());
        let session = sessions::open(&setup, &Choice::New, &mut notices).unwrap();
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
        let term = InlineTerminal::new(TestBackend::new(120, 40), 0).unwrap();
        let mut ui = Ui::start(agent, Box::new(host), term, options, approvals)
            .with_redactor(setup.redactor.clone());
        ui.draw().unwrap();
        send(&mut ui, "hello");
        settle(&mut ui).await;
        send(&mut ui, "/login chatgpt --device");
        settle(&mut ui).await;
        send(&mut ui, "/model chatgpt/gpt-5-codex");
        settle(&mut ui).await;
        send(&mut ui, "and now?");
        settle(&mut ui).await;
        let backend = ui.terminal().backend();
        let shown: String = [backend.scrollback(), backend.buffer()]
            .iter()
            .flat_map(|buffer| {
                buffer
                    .content
                    .chunks(buffer.area.width as usize)
                    .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        ui.finish().await.unwrap();
        sandbox_session.end();

        assert!(shown.contains("enter the code ABCD-1234"), "{shown}");
        assert!(
            shown.contains("Signed in to ChatGPT as dev@example.com"),
            "{shown}"
        );
        assert!(shown.contains("switched to chatgpt/gpt-5-codex"), "{shown}");
        assert!(shown.contains("Hi from ChatGPT."), "{shown}");
        // Neither token was shown.
        assert!(!shown.contains("rt-canary-0123456789"), "{shown}");
        assert!(!shown.contains(&access[10..40]), "{shown}");
        assert!(home.path().join("data/credentials.json").exists());
        // The conversation came along to ChatGPT.
        let requests = openai.received_requests().await.unwrap();
        let asked = requests
            .iter()
            .find(|r| r.url.path() == "/backend-api/codex/responses")
            .expect("ChatGPT was asked");
        let body = String::from_utf8_lossy(&asked.body);
        for said in ["hello", "Hello from the local model.", "and now?"] {
            assert!(body.contains(said), "{said}: {body}");
        }
    }
}
