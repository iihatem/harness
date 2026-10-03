//! Which server is used for a file, when it starts, where it runs, and when it is given up on.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use harness_core::{permission::FsAccess, tool::CommandSandbox};
use harness_lsp::{Launch, Manager, Report, ServerConsent, ServerSetting, Settings, language_of};

const SERVER: &str = env!("CARGO_BIN_EXE_fake-lsp-server");

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        Fixture { dir }
    }

    /// Puts the fake server on the fixture's `PATH` under `name`.
    fn install(&self, name: &str) {
        std::os::unix::fs::symlink(SERVER, self.dir.path().join("bin").join(name)).unwrap();
    }

    fn workspace(&self) -> PathBuf {
        self.dir.path().join("ws").canonicalize().unwrap()
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("server.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn count(&self, line: &str) -> usize {
        self.log().iter().filter(|l| *l == line).count()
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.workspace().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn settings(&self) -> Settings {
        Settings {
            enabled: true,
            wait: Duration::from_secs(5),
            first_wait: Duration::from_secs(10),
            servers: BTreeMap::new(),
            trusted: true,
            allowed: None,
            path: vec![self.dir.path().join("bin")],
            init_timeout: Duration::from_secs(10),
        }
    }

    fn manager(&self, settings: Settings) -> Manager {
        Manager::new(settings, self.workspace())
    }
}

/// A launch with nothing to hold servers back: run directly, as in `full-access`.
fn direct() -> Launch {
    Launch {
        sandbox: None,
        access: FsAccess::WorkspaceWrite,
        unsandboxed_ok: true,
    }
}

fn errors(report: &Report) -> usize {
    match report {
        Report::Checked(all) => all
            .iter()
            .filter(|d| d.severity == Some(harness_lsp::DiagnosticSeverity::ERROR))
            .count(),
        other => panic!("{other:?}"),
    }
}

// Spec "Server found": it starts at the first edit, not at session start.
#[tokio::test]
async fn a_server_on_path_starts_at_the_first_edit_and_not_before() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    assert!(f.log().is_empty());
    let report = manager
        .check(&f.file("main.go", "package main // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    assert_eq!(f.log()[0], "program: gopls");
    assert_eq!(f.count("initialize"), 1);
    manager.shutdown().await;
}

// Spec "Server missing": no section, and no error.
#[tokio::test]
async fn a_language_with_no_server_on_path_gets_nothing() {
    let f = Fixture::new();
    let manager = f.manager(f.settings());
    let report = manager
        .check(&f.file("main.py", "x = 1 # ERROR\n"), &direct())
        .await;
    assert_eq!(report, Report::NoServer);
    assert!(f.log().is_empty());
}

#[tokio::test]
async fn a_file_of_no_known_language_gets_nothing() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    assert_eq!(
        manager.check(&f.file("notes.txt", "x"), &direct()).await,
        Report::NoServer
    );
    assert!(f.log().is_empty());
}

#[test]
fn the_extensions_pick_the_languages() {
    for (file, language, id) in [
        ("a.rs", "rust", "rust"),
        ("a.ts", "typescript", "typescript"),
        ("a.tsx", "typescript", "typescriptreact"),
        ("a.js", "typescript", "javascript"),
        ("a.jsx", "typescript", "javascriptreact"),
        ("a.py", "python", "python"),
        ("a.go", "go", "go"),
    ] {
        let found = language_of(std::path::Path::new(file)).unwrap();
        assert_eq!(
            (found.name(), found.language_id()),
            (language, id),
            "{file}"
        );
    }
    assert!(language_of(std::path::Path::new("Makefile")).is_none());
}

// Spec: basedpyright-langserver, or else pyright-langserver --stdio, for Python.
#[tokio::test]
async fn python_prefers_basedpyright_and_falls_back_to_pyright() {
    let f = Fixture::new();
    f.install("pyright-langserver");
    let manager = f.manager(f.settings());
    manager.check(&f.file("a.py", "x\n"), &direct()).await;
    assert_eq!(
        f.log()[..2],
        ["program: pyright-langserver", "args: --stdio"]
    );
    manager.shutdown().await;

    let f = Fixture::new();
    f.install("pyright-langserver");
    f.install("basedpyright-langserver");
    let manager = f.manager(f.settings());
    manager.check(&f.file("a.py", "x\n"), &direct()).await;
    assert_eq!(f.log()[0], "program: basedpyright-langserver");
    manager.shutdown().await;
}

// Spec "Override".
#[tokio::test]
async fn an_override_in_the_settings_is_used_instead_of_the_lookup() {
    let f = Fixture::new();
    f.install("basedpyright-langserver");
    f.install("pylsp");
    let mut settings = f.settings();
    settings.servers.insert(
        "python".into(),
        ServerSetting {
            command: Some("pylsp --verbose".into()),
            enabled: true,
        },
    );
    let manager = f.manager(settings);
    manager.check(&f.file("a.py", "x\n"), &direct()).await;
    assert_eq!(f.log()[..2], ["program: pylsp", "args: --verbose"]);
    manager.shutdown().await;
}

#[tokio::test]
async fn an_override_with_a_path_is_run_from_there() {
    let f = Fixture::new();
    let mut settings = f.settings();
    settings.path = vec![];
    settings.servers.insert(
        "python".into(),
        ServerSetting {
            command: Some(format!("{}/bin/custom-server", f.dir.path().display())),
            enabled: true,
        },
    );
    f.install("custom-server");
    let manager = f.manager(settings);
    manager.check(&f.file("a.py", "x\n"), &direct()).await;
    assert_eq!(f.log()[0], "program: custom-server");
    manager.shutdown().await;
}

// Spec "Disabled".
#[tokio::test]
async fn disabled_servers_do_not_start() {
    let f = Fixture::new();
    f.install("gopls");
    f.install("rust-analyzer");
    let mut settings = f.settings();
    settings.servers.insert(
        "go".into(),
        ServerSetting {
            command: None,
            enabled: false,
        },
    );
    let manager = f.manager(settings.clone());
    assert_eq!(
        manager.check(&f.file("a.go", "x\n"), &direct()).await,
        Report::NoServer
    );
    // The other language still has its server.
    assert!(matches!(
        manager.check(&f.file("a.rs", "x\n"), &direct()).await,
        Report::Checked(_)
    ));
    manager.shutdown().await;

    let f = Fixture::new();
    f.install("gopls");
    settings.enabled = false;
    settings.path = vec![f.dir.path().join("bin")];
    let manager = f.manager(settings);
    assert_eq!(
        manager.check(&f.file("a.go", "x\n"), &direct()).await,
        Report::NoServer
    );
    assert!(f.log().is_empty());
}

/// Answers the question "start language servers here?" as scripted, and counts how often it is
/// asked.
struct Consent {
    answer: Option<bool>,
    asked: std::sync::atomic::AtomicUsize,
}

impl Consent {
    fn saying(answer: Option<bool>) -> Arc<Consent> {
        Arc::new(Consent {
            answer,
            asked: Default::default(),
        })
    }

    fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ServerConsent for Consent {
    async fn ask(&self) -> Option<bool> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.answer
    }
}

impl Fixture {
    /// A manager for a workspace that is not trusted, with `answer` stored and `consent` to ask.
    fn untrusted(&self, answer: Option<bool>, consent: Option<Arc<Consent>>) -> Manager {
        let mut settings = self.settings();
        settings.trusted = false;
        settings.allowed = answer;
        let manager = self.manager(settings);
        match consent {
            Some(consent) => manager.with_consent(consent),
            None => manager,
        }
    }
}

// Ruling P3: in an untrusted workspace the first edit of a file with a server asks once; a yes
// starts the server, and the second edit does not ask again.
#[tokio::test]
async fn the_first_edit_with_a_server_asks_once_and_a_yes_starts_it() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    let consent = Consent::saying(Some(true));
    let manager = f.untrusted(None, Some(consent.clone()));
    let report = manager
        .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    assert_eq!(consent.asked(), 1);
    let report = manager
        .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    assert_eq!(consent.asked(), 1);
    manager.shutdown().await;
}

// Declining starts nothing, says nothing, and is not asked again.
#[tokio::test]
async fn a_no_starts_no_server_and_is_not_asked_again() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    let consent = Consent::saying(Some(false));
    let manager = f.untrusted(None, Some(consent.clone()));
    for _ in 0..2 {
        assert_eq!(
            manager
                .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
                .await,
            Report::NoServer
        );
    }
    assert_eq!(consent.asked(), 1);
    assert!(f.log().is_empty());
}

// An answer that could not be had (nobody there) is no answer: nothing starts, and it is asked
// again at the next edit.
#[tokio::test]
async fn a_question_nobody_answered_starts_nothing() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    let consent = Consent::saying(None);
    let manager = f.untrusted(None, Some(consent.clone()));
    for _ in 0..2 {
        assert_eq!(
            manager
                .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
                .await,
            Report::NoServer
        );
    }
    assert_eq!(consent.asked(), 2);
    assert!(f.log().is_empty());
}

// The stored answer is the answer: a yes starts the server with no question, a no never does.
#[tokio::test]
async fn a_stored_answer_is_not_asked_again() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    let consent = Consent::saying(Some(false));
    let yes = f.untrusted(Some(true), Some(consent.clone()));
    let report = yes
        .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    yes.shutdown().await;
    let no = f.untrusted(Some(false), Some(consent.clone()));
    assert_eq!(
        no.check(&f.file("lib.rs", "x // ERROR\n"), &direct()).await,
        Report::NoServer
    );
    assert_eq!(consent.asked(), 0);
}

// Headless runs never ask: with no stored yes and no trust, no server starts, and there is no
// note either.
#[tokio::test]
async fn without_anyone_to_ask_an_unanswered_workspace_starts_no_server() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    let manager = f.untrusted(None, None);
    for _ in 0..2 {
        assert_eq!(
            manager
                .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
                .await,
            Report::NoServer
        );
    }
    assert!(f.log().is_empty());
}

// Nothing to ask about without a server for the file, and a trusted workspace is never asked.
#[tokio::test]
async fn no_question_without_a_server_or_in_a_trusted_workspace() {
    let f = Fixture::new();
    let consent = Consent::saying(Some(true));
    let manager = f.untrusted(None, Some(consent.clone()));
    assert_eq!(
        manager.check(&f.file("lib.rs", "x\n"), &direct()).await,
        Report::NoServer
    );
    f.install("rust-analyzer");
    let trusted = f.manager(f.settings()).with_consent(consent.clone());
    let report = trusted
        .check(&f.file("lib.rs", "x // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    trusted.shutdown().await;
    assert_eq!(consent.asked(), 0);
}

// Two files edited at once ask once.
#[tokio::test]
async fn files_checked_together_ask_once() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    f.install("gopls");
    let consent = Consent::saying(Some(true));
    let manager = f.untrusted(None, Some(consent.clone()));
    let (rs, go) = (
        f.file("lib.rs", "x // ERROR\n"),
        f.file("main.go", "x // ERROR\n"),
    );
    let launch = direct();
    let (a, b) = tokio::join!(manager.check(&rs, &launch), manager.check(&go, &launch));
    assert_eq!((errors(&a), errors(&b)), (1, 1));
    assert_eq!(consent.asked(), 1);
    manager.shutdown().await;
}

// With no sandbox to run it in, a server is not asked about: it could not start anyway.
#[tokio::test]
async fn no_question_when_no_server_could_start() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    let consent = Consent::saying(Some(true));
    let manager = f.untrusted(None, Some(consent.clone()));
    let no_sandbox = Launch {
        sandbox: None,
        access: FsAccess::WorkspaceWrite,
        unsandboxed_ok: false,
    };
    let report = manager.check(&f.file("lib.rs", "x\n"), &no_sandbox).await;
    assert!(matches!(report, Report::Note(_)), "{report:?}");
    assert_eq!(consent.asked(), 0);
}

/// Starts what it is asked to through `/bin/sh -c exec`, as a sandbox would wrap a command, and
/// records the access it was asked to give.
#[derive(Debug, Default)]
struct Recording {
    asked: Mutex<Vec<(FsAccess, String, Vec<String>)>>,
}

impl CommandSandbox for Recording {
    fn name(&self) -> &'static str {
        "recording"
    }
    fn command(
        &self,
        access: FsAccess,
        _workspace: &std::path::Path,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<tokio::process::Command> {
        self.asked.lock().unwrap().push((
            access,
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        let mut command = tokio::process::Command::new(program);
        command.args(args).process_group(0);
        Ok(command)
    }
    fn is_denial(&self, _exit_code: Option<i32>, _output: &str) -> bool {
        false
    }
}

// Spec "Sandboxed server": the server is started through the same sandbox as bash.
#[tokio::test]
async fn a_server_is_started_through_the_sandbox_with_the_modes_access() {
    let f = Fixture::new();
    f.install("gopls");
    let sandbox = Arc::new(Recording::default());
    let manager = f.manager(f.settings());
    let launch = Launch {
        sandbox: Some(sandbox.clone()),
        access: FsAccess::WorkspaceWrite,
        unsandboxed_ok: false,
    };
    let report = manager
        .check(&f.file("a.go", "x // ERROR\n"), &launch)
        .await;
    assert_eq!(errors(&report), 1);
    let asked = sandbox.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, FsAccess::WorkspaceWrite);
    assert!(asked[0].1.ends_with("/gopls"), "{asked:?}");
    manager.shutdown().await;
}

// Without a sandbox, and not in full-access, a server does not start: one note says why.
#[tokio::test]
async fn without_a_sandbox_a_server_does_not_start_unless_the_mode_allows_it() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    let none = Launch {
        sandbox: None,
        access: FsAccess::WorkspaceWrite,
        unsandboxed_ok: false,
    };
    let first = manager.check(&f.file("a.go", "x\n"), &none).await;
    let Report::Note(note) = first else {
        panic!("{first:?}")
    };
    assert!(note.contains("sandbox"), "{note}");
    assert_eq!(
        manager.check(&f.file("a.go", "x\n"), &none).await,
        Report::NoServer
    );
    assert!(f.log().is_empty());
    // Full-access runs it directly.
    assert!(matches!(
        manager.check(&f.file("a.go", "x\n"), &direct()).await,
        Report::Checked(_)
    ));
    manager.shutdown().await;
}

// Spec "Repeated crashes": it is started again after the first, and not after the second.
#[tokio::test]
async fn a_server_that_crashes_twice_is_not_started_again() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    let crash = f.file("a.go", "CRASH\n");
    assert_eq!(manager.check(&crash, &direct()).await, Report::NoServer);
    // The second crash is the last: the user is told once, so that the silence is not mistaken
    // for a clean file.
    let Report::Note(note) = manager.check(&crash, &direct()).await else {
        panic!("a note was expected")
    };
    assert!(note.contains("go") && note.contains("/new"), "{note}");
    assert_eq!(f.count("initialize"), 2);
    // A clean file now gets nothing, and the server is not started a third time.
    let clean = f.file("b.go", "package b\n");
    assert_eq!(manager.check(&clean, &direct()).await, Report::NoServer);
    assert_eq!(f.count("initialize"), 2);
}

#[tokio::test]
async fn one_crash_is_forgiven_and_the_server_comes_back() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    assert_eq!(
        manager.check(&f.file("a.go", "CRASH\n"), &direct()).await,
        Report::NoServer
    );
    let report = manager
        .check(&f.file("a.go", "x // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    assert_eq!(f.count("initialize"), 2);
    manager.shutdown().await;
}

// Spec: 10 s for the first request while the server indexes, then `wait`.
#[tokio::test]
async fn the_first_request_is_given_longer_than_the_later_ones() {
    let f = Fixture::new();
    f.install("gopls");
    let mut settings = f.settings();
    settings.wait = Duration::from_millis(300);
    settings.first_wait = Duration::from_secs(5);
    let manager = f.manager(settings);
    let slow = f.file("a.go", "SLOW:1200\nx // ERROR\n");
    assert_eq!(errors(&manager.check(&slow, &direct()).await), 1);
    // Too late for the second: the set known from the first is given back, marked, not "pending".
    assert!(matches!(
        manager.check(&slow, &direct()).await,
        Report::Unchanged(_)
    ));
    manager.shutdown().await;
}

// Spec "New session": every running server is stopped.
#[tokio::test]
async fn resetting_stops_every_server_and_the_next_edit_starts_afresh() {
    let f = Fixture::new();
    f.install("gopls");
    f.install("rust-analyzer");
    let manager = f.manager(f.settings());
    manager.check(&f.file("a.go", "x\n"), &direct()).await;
    manager.check(&f.file("a.rs", "x\n"), &direct()).await;
    assert_eq!(f.count("initialize"), 2);
    manager.reset();
    for _ in 0..100 {
        if f.count("exit") == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        (f.count("shutdown"), f.count("exit")),
        (2, 2),
        "{:?}",
        f.log()
    );
    manager.check(&f.file("a.go", "x\n"), &direct()).await;
    assert_eq!(f.count("initialize"), 3);
    manager.shutdown().await;
}

#[tokio::test]
async fn a_reset_forgives_crashes_and_notes_again() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    let crash = f.file("a.go", "CRASH\n");
    manager.check(&crash, &direct()).await;
    manager.check(&crash, &direct()).await;
    manager.reset();
    let report = manager
        .check(&f.file("a.go", "x // ERROR\n"), &direct())
        .await;
    assert_eq!(errors(&report), 1);
    manager.shutdown().await;
}

#[tokio::test]
async fn a_file_that_cannot_be_read_gets_nothing() {
    let f = Fixture::new();
    f.install("gopls");
    let manager = f.manager(f.settings());
    let gone = f.workspace().join("gone.go");
    assert_eq!(manager.check(&gone, &direct()).await, Report::NoServer);
    let binary = f.workspace().join("bin.go");
    std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
    assert_eq!(manager.check(&binary, &direct()).await, Report::NoServer);
    assert!(f.log().is_empty());
}

// rust-analyzer's `cargo check` writes `target/`, which a read-only workspace does not allow.
#[tokio::test]
async fn rust_analyzer_is_not_started_in_read_only_access_and_the_user_is_told_once() {
    let f = Fixture::new();
    f.install("rust-analyzer");
    f.install("gopls");
    let manager = f.manager(f.settings());
    let read_only = Launch {
        sandbox: None,
        access: FsAccess::ReadOnly,
        unsandboxed_ok: true,
    };
    let file = f.file("a.rs", "x // ERROR\n");
    let Report::Note(note) = manager.check(&file, &read_only).await else {
        panic!("a note was expected")
    };
    assert!(note.contains("read-only"), "{note}");
    assert_eq!(manager.check(&file, &read_only).await, Report::NoServer);
    assert_eq!(f.count("initialize"), 0);
    // Other servers do not write to the workspace.
    let report = manager
        .check(&f.file("a.go", "x // ERROR\n"), &read_only)
        .await;
    assert_eq!(errors(&report), 1);
    manager.shutdown().await;
}
