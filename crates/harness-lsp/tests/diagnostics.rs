//! What is appended to an edit result: the errors in the edited files, capped, or a note.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use harness_core::{
    diag::{Diagnostics, EditedFiles},
    permission::FsAccess,
};
use harness_lsp::{LspDiagnostics, Manager, Settings};

const SERVER: &str = env!("CARGO_BIN_EXE_fake-lsp-server");

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::create_dir_all(dir.path().join("ws/src")).unwrap();
        for name in ["typescript-language-server", "gopls"] {
            std::os::unix::fs::symlink(SERVER, dir.path().join("bin").join(name)).unwrap();
        }
        Fixture { dir }
    }

    fn workspace(&self) -> PathBuf {
        self.dir.path().join("ws").canonicalize().unwrap()
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.workspace().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn diagnostics(&self, wait: Duration, trusted: bool) -> LspDiagnostics {
        LspDiagnostics::new(
            Manager::new(
                Settings {
                    enabled: true,
                    wait,
                    first_wait: wait,
                    servers: BTreeMap::new(),
                    trusted,
                    allowed: None,
                    path: vec![self.dir.path().join("bin")],
                    init_timeout: Duration::from_secs(10),
                },
                self.workspace(),
            ),
            self.workspace(),
        )
    }
}

async fn after_edit(d: &LspDiagnostics, paths: &[PathBuf]) -> Option<String> {
    d.after_edit(&EditedFiles {
        paths,
        workspace: &paths[0].parent().unwrap().canonicalize().unwrap(),
        sandbox: None,
        access: FsAccess::WorkspaceWrite,
        unsandboxed_ok: true,
    })
    .await
}

// Spec "Type error": the error is listed with `7` and its message.
#[tokio::test]
async fn an_error_is_listed_with_its_file_line_and_message() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    let text = format!("{}let n: number = 's'; // ERROR\n", "\n".repeat(6));
    let said = after_edit(&d, &[f.file("a.ts", &text)]).await.unwrap();
    assert!(said.contains("1 error"), "{said}");
    assert!(said.contains("a.ts:7: problem on line 7"), "{said}");
    d.shutdown().await;
}

// Spec "Cap": 35 errors, 20 listed.
#[tokio::test]
async fn at_most_20_errors_are_listed_and_the_rest_counted() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    let text: String = (1..=35).map(|n| format!("e{n} // ERROR\n")).collect();
    let said = after_edit(&d, &[f.file("a.ts", &text)]).await.unwrap();
    let listed = said.lines().filter(|l| l.starts_with("a.ts:")).count();
    assert_eq!(listed, 20, "{said}");
    assert!(said.contains("35 errors"), "{said}");
    assert!(said.contains("15 more errors not shown"), "{said}");
    assert!(
        said.contains("a.ts:20:") && !said.contains("a.ts:21:"),
        "{said}"
    );
    d.shutdown().await;
}

// Only errors: warnings, information and hints are not appended.
#[tokio::test]
async fn warnings_and_hints_are_left_out() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    let said = after_edit(&d, &[f.file("a.ts", "a // WARN\nb // HINT\nc // ERROR\n")])
        .await
        .unwrap();
    assert!(said.contains("a.ts:3:") && !said.contains("a.ts:1:") && !said.contains("a.ts:2:"));
    assert!(
        after_edit(&d, &[f.file("a.ts", "a // WARN\nb // HINT\n")])
            .await
            .is_none()
    );
    d.shutdown().await;
}

// Spec "Timeout": "diagnostics pending", never "no errors".
#[tokio::test]
async fn a_server_that_is_late_leaves_a_pending_note_and_no_claim_of_no_errors() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_millis(300), true);
    let said = after_edit(&d, &[f.file("a.ts", "NOPUBLISH\n")])
        .await
        .unwrap();
    assert!(said.contains("diagnostics pending"), "{said}");
    assert!(!said.to_lowercase().contains("no errors"), "{said}");
    d.shutdown().await;
}

// Spec "Clean file": nothing about errors.
#[tokio::test]
async fn a_clean_file_adds_nothing() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    assert!(
        after_edit(&d, &[f.file("a.ts", "let n = 1;\n")])
            .await
            .is_none()
    );
    d.shutdown().await;
}

// No server for the language: no section, and no error.
#[tokio::test]
async fn a_language_without_a_server_adds_nothing() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    assert!(
        after_edit(&d, &[f.file("a.py", "x // ERROR\n")])
            .await
            .is_none()
    );
}

// Ruling P3: where nobody can be asked (a headless run), an unanswered, untrusted workspace gets
// no diagnostics and no note: the question replaced the note.
#[tokio::test]
async fn an_unanswered_untrusted_workspace_with_nobody_to_ask_says_nothing() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), false);
    for _ in 0..2 {
        assert!(
            after_edit(&d, &[f.file("a.ts", "x // ERROR\n")])
                .await
                .is_none()
        );
    }
}

// A tool that changes several files (a patch) is checked file by file, errors listed together.
#[tokio::test]
async fn several_files_are_checked_and_listed_together() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    let said = after_edit(
        &d,
        &[
            f.file("a.ts", "x // ERROR\n"),
            f.file("b.go", "y // ERROR\n"),
            f.file("c.txt", "z // ERROR\n"),
        ],
    )
    .await
    .unwrap();
    assert!(said.contains("2 errors"), "{said}");
    assert!(
        said.contains("a.ts:1:") && said.contains("b.go:1:"),
        "{said}"
    );
    assert!(!said.contains("c.txt"), "{said}");
    d.shutdown().await;
}

// A path that is gone (a deleted file) has nothing to check.
#[tokio::test]
async fn a_deleted_file_adds_nothing() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    let gone = f.workspace().join("gone.ts");
    let present = f.file("a.ts", "ok\n");
    assert!(after_edit(&d, &[present, gone]).await.is_none());
    d.shutdown().await;
}

// A server's message cannot smuggle terminal escapes or extra lines into the result.
#[tokio::test]
async fn a_message_is_one_clean_line() {
    let f = Fixture::new();
    let d = f.diagnostics(Duration::from_secs(5), true);
    let said = after_edit(&d, &[f.file("a.ts", "x \u{1b}[31m // ERROR\n")])
        .await
        .unwrap();
    assert!(!said.contains('\u{1b}'), "{said:?}");
    d.shutdown().await;
}
