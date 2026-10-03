//! The client against a fake language server (`src/bin/fake-lsp-server.rs`).

use std::{path::Path, time::Duration};

use harness_lsp::{Check, Client, DiagnosticSeverity, LspError, uri_of};
use tokio::process::Command;

const SERVER: &str = env!("CARGO_BIN_EXE_fake-lsp-server");
const WAIT: Duration = Duration::from_secs(5);

struct Fixture {
    dir: tempfile::TempDir,
    log: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("server.log");
    Fixture { dir, log }
}

impl Fixture {
    async fn start(&self) -> Client {
        let mut command = Command::new(SERVER);
        command.env("FAKE_LSP_LOG", &self.log).arg("--stdio");
        Client::start(command, self.dir.path(), Duration::from_secs(10))
            .await
            .unwrap()
    }

    fn file(&self) -> std::path::PathBuf {
        self.dir.path().join("src lib.rs")
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

async fn check(client: &Client, f: &Fixture, text: &str, wait: Duration) -> Check {
    client.check(&f.file(), "rust", text, wait).await.unwrap()
}

#[tokio::test]
async fn it_initializes_the_server_and_answers_its_requests() {
    let f = fixture();
    let client = f.start().await;
    // Let the server's own request (progress creation) be answered, then it sees the reply.
    check(&client, &f, "fn main() {}\n", WAIT).await;
    let log = f.log();
    assert_eq!(log[1], "args: --stdio");
    assert_eq!(log[2], "initialize");
    assert!(log.contains(&"initialized".to_string()), "{log:?}");
    assert!(log.contains(&"reply".to_string()), "{log:?}");
    client.shutdown().await;
}

// Spec "Type error": the error is listed with its line.
#[tokio::test]
async fn an_error_the_server_publishes_comes_back_with_its_line() {
    let f = fixture();
    let client = f.start().await;
    let text = "fn main() {\n\n\n\n\n\n    let x: i32 = \"a\"; // ERROR\n}\n";
    let result = check(&client, &f, text, WAIT).await;
    let errors = result.errors();
    assert_eq!(errors.len(), 1, "{result:?}");
    assert_eq!(errors[0].range.start.line, 6);
    assert!(
        errors[0].message.contains("line 7"),
        "{}",
        errors[0].message
    );
    client.shutdown().await;
}

#[tokio::test]
async fn warnings_and_hints_are_published_but_are_not_errors() {
    let f = fixture();
    let client = f.start().await;
    let result = check(&client, &f, "a // WARN\nb // HINT\nc // ERROR\n", WAIT).await;
    let Check::Published(all) = &result else {
        panic!("{result:?}")
    };
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(result.errors().len(), 1);
    client.shutdown().await;
}

// Spec "Clean file": published, with no errors.
#[tokio::test]
async fn a_clean_file_is_published_with_no_diagnostics() {
    let f = fixture();
    let client = f.start().await;
    assert_eq!(
        check(&client, &f, "fn main() {}\n", WAIT).await,
        Check::Published(Vec::new())
    );
    client.shutdown().await;
}

// Spec "Timeout": nothing published in time is "pending", not "clean".
#[tokio::test]
async fn a_server_that_publishes_nothing_leaves_the_check_pending() {
    let f = fixture();
    let client = f.start().await;
    let started = std::time::Instant::now();
    let result = check(&client, &f, "NOPUBLISH\n", Duration::from_millis(300)).await;
    assert_eq!(result, Check::Pending);
    assert!(started.elapsed() < Duration::from_secs(3));
    client.shutdown().await;
}

#[tokio::test]
async fn a_slow_server_is_waited_for_up_to_the_wait_and_no_longer() {
    let f = fixture();
    let client = f.start().await;
    let text = "SLOW:400\nx // ERROR\n";
    assert_eq!(
        check(&client, &f, text, Duration::from_millis(100)).await,
        Check::Pending
    );
    // The publish that was late still comes, and the next check is not fooled by it: it waits for
    // a publish of its own text.
    let result = check(&client, &f, "fresh // ERROR\n", WAIT).await;
    assert_eq!(result.errors().len(), 1, "{result:?}");
    assert!(result.errors()[0].message.contains("fresh"), "{result:?}");
    client.shutdown().await;
}

// The first check opens the file; later ones replace its text, and never get an earlier text's
// diagnostics.
#[tokio::test]
async fn later_checks_change_the_file_and_get_fresh_diagnostics() {
    let f = fixture();
    let client = f.start().await;
    assert_eq!(
        check(&client, &f, "a // ERROR\n", WAIT)
            .await
            .errors()
            .len(),
        1
    );
    assert_eq!(
        check(&client, &f, "a\n", WAIT).await,
        Check::Published(Vec::new())
    );
    let log = f.log();
    let opens = log.iter().filter(|m| *m == "textDocument/didOpen").count();
    let changes = log
        .iter()
        .filter(|m| *m == "textDocument/didChange")
        .count();
    assert_eq!((opens, changes), (1, 1), "{log:?}");
    client.shutdown().await;
}

// A server publishes a syntax pass and then the type pass: the settled result is the last.
#[tokio::test]
async fn a_second_publish_right_after_the_first_is_the_one_returned() {
    let f = fixture();
    let client = f.start().await;
    let result = check(&client, &f, "TWICE\nx // ERROR\n", WAIT).await;
    assert_eq!(result.errors().len(), 1, "{result:?}");
    client.shutdown().await;
}

#[tokio::test]
async fn a_server_that_exits_is_reported_and_stays_exited() {
    let f = fixture();
    let client = f.start().await;
    assert_eq!(check(&client, &f, "CRASH\n", WAIT).await, Check::Exited);
    assert!(client.has_exited());
    // Checks after that fail without waiting for the wait.
    let started = std::time::Instant::now();
    let later = client.check(&f.file(), "rust", "x\n", WAIT).await;
    assert!(
        matches!(later, Err(LspError::Exited) | Ok(Check::Exited)),
        "{later:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    client.shutdown().await;
}

#[tokio::test]
async fn shutdown_asks_the_server_to_shut_down_and_exit() {
    let f = fixture();
    let client = f.start().await;
    client.shutdown().await;
    let log = f.log();
    assert_eq!(&log[log.len() - 2..], ["shutdown", "exit"], "{log:?}");
}

#[tokio::test]
async fn a_program_that_is_not_there_cannot_start() {
    let dir = tempfile::tempdir().unwrap();
    let result = Client::start(
        Command::new("/no/such/language-server"),
        dir.path(),
        Duration::from_secs(1),
    )
    .await;
    assert!(matches!(result, Err(LspError::Spawn(_))));
}

#[tokio::test]
async fn a_server_that_never_answers_initialize_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new("sleep");
    command.arg("30");
    let started = std::time::Instant::now();
    let result = Client::start(command, dir.path(), Duration::from_millis(300)).await;
    assert!(matches!(result, Err(LspError::InitializeTimeout)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_path_becomes_a_percent_encoded_file_uri() {
    assert_eq!(uri_of(Path::new("/a/b.rs")), "file:///a/b.rs");
    assert_eq!(
        uri_of(Path::new("/a b/é#.rs")),
        "file:///a%20b/%C3%A9%23.rs"
    );
}
