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

// rust-analyzer's way: an empty set at once, while it is still indexing, and the real one later.
// The empty set is not the answer.
#[tokio::test]
async fn an_empty_publish_while_the_server_reports_progress_is_not_the_answer() {
    let f = fixture();
    let client = f.start().await;
    let started = std::time::Instant::now();
    let result = check(&client, &f, "RALIKE\nx // ERROR\n", WAIT).await;
    assert_eq!(result.errors().len(), 1, "{result:?}");
    assert!(started.elapsed() >= Duration::from_millis(1100));
    // The next edit, with no progress, is answered at once.
    let started = std::time::Instant::now();
    let clean = check(&client, &f, "fine\n", WAIT).await;
    assert_eq!(clean, Check::Published(Vec::new()));
    assert!(started.elapsed() < Duration::from_millis(900));
    client.shutdown().await;
}

#[tokio::test]
async fn a_server_status_that_is_not_quiescent_holds_the_answer_too() {
    let f = fixture();
    let client = f.start().await;
    let started = std::time::Instant::now();
    let result = check(&client, &f, "STATUS\nx // ERROR\n", WAIT).await;
    assert_eq!(result.errors().len(), 1, "{result:?}");
    assert!(started.elapsed() >= Duration::from_millis(1100));
    client.shutdown().await;
}

// rust-analyzer says it is idle, and only then starts `cargo check`: the first request waits that
// long, once.
#[tokio::test]
async fn the_first_check_waits_for_work_that_starts_just_after_the_server_is_idle() {
    let f = fixture();
    let client = f.start().await;
    let result = check(&client, &f, "FLYCHECK\nx // ERROR\n", WAIT).await;
    assert_eq!(result.errors().len(), 1, "{result:?}");
    // Afterwards a clean file is not held up for it.
    let started = std::time::Instant::now();
    assert_eq!(
        check(&client, &f, "fine\n", WAIT).await,
        Check::Published(Vec::new())
    );
    assert!(started.elapsed() < Duration::from_millis(900));
    client.shutdown().await;
}

// A server whose progress never ends degrades to the latest publish at the end of the wait.
#[tokio::test]
async fn progress_that_never_ends_gives_the_latest_publish_at_the_end_of_the_wait() {
    let f = fixture();
    let client = f.start().await;
    let started = std::time::Instant::now();
    let result = check(
        &client,
        &f,
        "STUCK\nx // ERROR\n",
        Duration::from_millis(700),
    )
    .await;
    assert_eq!(result.errors().len(), 1, "{result:?}");
    assert!(started.elapsed() >= Duration::from_millis(600));
    assert!(started.elapsed() < Duration::from_secs(3));
    client.shutdown().await;
}

// rust-analyzer starts `cargo check` on a save.
#[tokio::test]
async fn every_check_saves_the_file_after_telling_the_server_its_text() {
    let f = fixture();
    let client = f.start().await;
    check(&client, &f, "a\n", WAIT).await;
    check(&client, &f, "b\n", WAIT).await;
    let log = f.log();
    let tail: Vec<&str> = log
        .iter()
        .map(String::as_str)
        .filter(|m| m.starts_with("textDocument/"))
        .collect();
    assert_eq!(
        tail,
        [
            "textDocument/didOpen",
            "textDocument/didSave",
            "textDocument/didChange",
            "textDocument/didSave"
        ]
    );
    client.shutdown().await;
}

// A publish under an older version of the file is not the answer for this edit.
#[tokio::test]
async fn a_publish_for_an_older_version_is_not_taken_as_fresh() {
    let f = fixture();
    let client = f.start().await;
    assert_eq!(
        check(&client, &f, "a\n", WAIT).await,
        Check::Published(Vec::new())
    );
    let result = check(
        &client,
        &f,
        "STALE\nold // ERROR\n",
        Duration::from_millis(500),
    )
    .await;
    assert!(matches!(result, Check::Unchanged(_)), "{result:?}");
    // A publish with no version keeps the rule that it came after the change.
    let result = check(&client, &f, "NOVERSION\nnew // ERROR\n", WAIT).await;
    assert!(matches!(result, Check::Published(_)), "{result:?}");
    client.shutdown().await;
}

// A server that does not republish an unchanged set: the known set comes back, marked.
#[tokio::test]
async fn an_unchanged_set_that_is_not_republished_comes_back_marked_unchanged() {
    let f = fixture();
    let client = f.start().await;
    let first = check(&client, &f, "NOREPEAT\nx // ERROR\n", WAIT).await;
    assert_eq!(first.errors().len(), 1, "{first:?}");
    let started = std::time::Instant::now();
    let second = check(
        &client,
        &f,
        "NOREPEAT\nx // ERROR\n\n",
        Duration::from_millis(400),
    )
    .await;
    let Check::Unchanged(set) = &second else {
        panic!("{second:?}")
    };
    assert_eq!(set.len(), 1);
    assert_eq!(second.errors().len(), 1);
    assert!(started.elapsed() < Duration::from_secs(3));
    client.shutdown().await;
}

// A server may write the percent-encoding in another case.
#[tokio::test]
async fn a_uri_the_server_encodes_differently_still_matches_the_file() {
    let f = fixture();
    let client = f.start().await;
    let path = f.dir.path().join("é.rs");
    let result = client
        .check(&path, "rust", "LOWERHEX\nx // ERROR\n", WAIT)
        .await
        .unwrap();
    assert_eq!(result.errors().len(), 1, "{result:?}");
    client.shutdown().await;
}

fn alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

async fn child_of(file: &std::path::Path) -> i32 {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(file)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the server wrote no child pid");
}

async fn dies(pid: i32) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// Servers run in a process group of their own, as the manager starts them; what they started
// (cargo check, tsserver) goes with them when they are ended without a goodbye.
#[tokio::test]
async fn a_descendant_of_a_dropped_server_dies_with_it() {
    let f = fixture();
    let child = f.dir.path().join("child.pid");
    let mut command = Command::new(SERVER);
    command
        .env("FAKE_LSP_LOG", &f.log)
        .env("FAKE_LSP_CHILD", &child)
        .arg("--stdio")
        .process_group(0);
    let client = Client::start(command, f.dir.path(), Duration::from_secs(10))
        .await
        .unwrap();
    let pid = child_of(&child).await;
    assert!(alive(pid));
    drop(client);
    assert!(dies(pid).await, "the descendant {pid} is still running");
}

#[tokio::test]
async fn a_descendant_of_a_server_that_never_initializes_dies_with_it() {
    let f = fixture();
    let child = f.dir.path().join("child.pid");
    let mut command = Command::new(SERVER);
    command
        .env("FAKE_LSP_LOG", &f.log)
        .env("FAKE_LSP_CHILD", &child)
        .env("FAKE_LSP_HANG_INIT", "1")
        .arg("--stdio")
        .process_group(0);
    let result = Client::start(command, f.dir.path(), Duration::from_secs(2)).await;
    assert!(matches!(result, Err(LspError::InitializeTimeout)));
    let pid = child_of(&child).await;
    assert!(dies(pid).await, "the descendant {pid} is still running");
}

#[test]
fn a_path_becomes_a_percent_encoded_file_uri() {
    assert_eq!(uri_of(Path::new("/a/b.rs")), "file:///a/b.rs");
    assert_eq!(
        uri_of(Path::new("/a b/é#.rs")),
        "file:///a%20b/%C3%A9%23.rs"
    );
}
