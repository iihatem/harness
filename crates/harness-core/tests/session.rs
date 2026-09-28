use std::path::Path;

use harness_core::message::Message;
use harness_core::session::{self, EntryKind, Session, SessionError};

fn user(text: &str) -> EntryKind {
    EntryKind::Message {
        message: Message::User {
            content: text.into(),
        },
        display: None,
        note: false,
    }
}

fn assistant(text: &str) -> EntryKind {
    EntryKind::Message {
        message: Message::Assistant {
            content: text.into(),
            tool_calls: vec![],
            model: "mock/m".into(),
        },
        display: None,
        note: false,
    }
}

fn texts(session: &Session) -> Vec<String> {
    session
        .messages()
        .into_iter()
        .map(|(_, m)| match m {
            Message::User { content } | Message::Assistant { content, .. } => content,
            Message::Tool { content, .. } => content,
        })
        .collect()
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn nothing_is_written_until_the_first_entry() {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::create(dir.path(), Path::new("/work"));
    assert!(!session.path().unwrap().exists());
    assert!(session::list(dir.path()).is_empty());
}

// Spec: entries survive a crash. Each entry is on disk as soon as `append` returns.
#[test]
fn each_entry_is_on_disk_as_soon_as_it_is_appended() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), Path::new("/work"));
    session.append(user("one"));
    session.append(assistant("answer one"));
    let path = session.path().unwrap().to_path_buf();
    let written = lines(&path);
    assert_eq!(written.len(), 3);
    assert!(written[0].contains(r#""type":"session""#), "{}", written[0]);
    assert!(written[1].contains(r#""type":"message""#) && written[1].contains("one"));
    // Every entry names its parent.
    let header: serde_json::Value = serde_json::from_str(&written[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(&written[2]).unwrap();
    assert_eq!(header["parent_id"], serde_json::Value::Null);
    assert!(second["id"].is_string() && second["parent_id"].is_string());
    std::mem::forget(session);
    assert_eq!(lines(&path).len(), 3);
}

// Spec: branch after rewind. Appending under an earlier entry leaves the old branch in the file.
#[test]
fn a_new_branch_keeps_the_old_one_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), Path::new("/work"));
    session.append(user("one"));
    let first_answer = session.append(assistant("answer one"));
    session.append(user("three"));
    session.append(assistant("answer three"));
    session.append_under(&first_answer, user("three, differently"));
    assert_eq!(texts(&session), ["one", "answer one", "three, differently"]);
    let path = session.path().unwrap().to_path_buf();
    drop(session);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"three\"") && text.contains("answer three"));
    let (reopened, warnings) = Session::open(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        texts(&reopened),
        ["one", "answer one", "three, differently"]
    );
}

// Spec: truncated session files are tolerated.
#[test]
fn an_incomplete_last_line_is_dropped_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), Path::new("/work"));
    session.append(user("one"));
    session.append(assistant("answer one"));
    let path = session.path().unwrap().to_path_buf();
    drop(session);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(r#"{"id":"abc","parent_id":"#);
    std::fs::write(&path, text).unwrap();

    let (mut session, warnings) = Session::open(&path).unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("incomplete"), "{warnings:?}");
    assert_eq!(texts(&session), ["one", "answer one"]);
    // Appending starts on a fresh line.
    session.append(user("two"));
    drop(session);
    let (session, warnings) = Session::open(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(texts(&session), ["one", "answer one", "two"]);
}

#[test]
fn an_unreadable_line_in_the_middle_is_skipped_with_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), Path::new("/work"));
    session.append(user("one"));
    let path = session.path().unwrap().to_path_buf();
    drop(session);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("not json\n");
    std::fs::write(&path, text).unwrap();
    let (session, warnings) = Session::open(&path).unwrap();
    assert!(warnings[0].contains("1 unreadable line"), "{warnings:?}");
    assert_eq!(texts(&session), ["one"]);
}

// Review Focus: two harness processes must never append to the same session.
#[test]
fn a_session_open_elsewhere_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), Path::new("/work"));
    session.append(user("one"));
    let path = session.path().unwrap().to_path_buf();
    assert!(matches!(Session::open(&path), Err(SessionError::InUse(_))));
    drop(session);
    let (_reopened, _) = Session::open(&path).unwrap();
    assert!(matches!(Session::open(&path), Err(SessionError::InUse(_))));
}

#[test]
fn a_file_that_is_not_a_session_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.jsonl");
    std::fs::write(&path, "{\"hello\":1}\n").unwrap();
    assert!(matches!(
        Session::open(&path),
        Err(SessionError::NotASession(_))
    ));
}

#[test]
fn sessions_are_listed_most_recent_first_with_their_first_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut older = Session::create(dir.path(), Path::new("/work"));
    older.append(EntryKind::Message {
        message: Message::User {
            content: "the expanded command".into(),
        },
        display: Some("/opsx:propose add-login".into()),
        note: false,
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut newer = Session::create(dir.path(), Path::new("/work"));
    newer.append(EntryKind::Message {
        message: Message::User {
            content: "[harness] note".into(),
        },
        display: None,
        note: true,
    });
    newer.append(user("fix the tests"));
    let listed = session::list(dir.path());
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, newer.id());
    assert_eq!(listed[0].first_message.as_deref(), Some("fix the tests"));
    assert_eq!(
        listed[1].first_message.as_deref(),
        Some("/opsx:propose add-login")
    );
    assert!(
        listed[1].started_at.ends_with('Z'),
        "{}",
        listed[1].started_at
    );
}

#[test]
fn a_session_that_cannot_be_saved_keeps_working_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("not-a-directory");
    std::fs::write(&blocked, "").unwrap();
    let mut session = Session::create(&blocked, Path::new("/work"));
    session.append(user("one"));
    let error = session
        .take_save_error()
        .expect("the file cannot be created");
    assert!(!error.to_string().is_empty());
    session.append(assistant("answer one"));
    assert!(session.take_save_error().is_none(), "reported only once");
    assert_eq!(texts(&session), ["one", "answer one"]);
    assert_eq!(session.path(), None);
}

// Review D I2: sessions hold prompts, code and tool output, so only their owner may read them.
#[test]
fn session_files_and_their_folders_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let project = sessions.join("project-key");
    let mut session = Session::create(&project, Path::new("/work"));
    session.append(user("one"));
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(session.path().unwrap()), 0o600);
    assert_eq!(mode(&project), 0o700);
    assert_eq!(mode(&sessions), 0o700);
}

/// A session file at `dir/<name>.jsonl` whose first line gives the session id `id`.
fn session_file(dir: &Path, name: &str, id: &str) -> std::path::PathBuf {
    let path = dir.join(format!("{name}.jsonl"));
    let header = serde_json::json!({
        "id": id,
        "parent_id": null,
        "type": "session",
        "version": 1,
        "started_at": "2026-01-02T00:00:00Z",
        "cwd": "/work",
    });
    let message = serde_json::json!({
        "id": "0000000a",
        "parent_id": id,
        "type": "message",
        "message": {"role": "user", "content": "hello"},
    });
    std::fs::write(&path, format!("{header}\n{message}\n")).unwrap();
    path
}

// Review D I3: the id inside a session file becomes part of paths (the checkpoint index), so
// it must be the file's own name, and a safe one.
#[test]
fn a_session_whose_id_is_not_its_file_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(
        dir.path(),
        "20260102T000000Z-cafe",
        "../../PWNED-by-header-id",
    );
    assert!(matches!(
        Session::open(&path),
        Err(SessionError::NotASession(_))
    ));
    let path = session_file(dir.path(), "20260102T000000Z-beef", "20260102T000000Z-cafe");
    assert!(matches!(
        Session::open(&path),
        Err(SessionError::NotASession(_))
    ));
    // Neither is listed, so `-c` never picks one.
    assert!(session::list(dir.path()).is_empty());
    let path = session_file(dir.path(), "20260102T000000Z-f00d", "20260102T000000Z-f00d");
    let (session, _) = Session::open(&path).unwrap();
    assert_eq!(session.id(), "20260102T000000Z-f00d");
    drop(session);
    let listed = session::list(dir.path());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "20260102T000000Z-f00d");
}

#[test]
fn a_session_whose_name_is_not_a_safe_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["with space", "dot.ted", &"a".repeat(65)] {
        let path = session_file(dir.path(), name, name);
        assert!(
            matches!(Session::open(&path), Err(SessionError::NotASession(_))),
            "{name}"
        );
    }
    assert!(session::list(dir.path()).is_empty());
}

#[test]
fn session_ids_are_letters_digits_and_dashes() {
    assert!(session::is_valid_id("20260927T123456Z-1a2b3c4d"));
    assert!(session::is_valid_id(&"a".repeat(64)));
    for id in [
        "",
        "../x",
        "/etc/passwd",
        "a b",
        "a.b",
        "a_b",
        &"a".repeat(65),
        "é",
    ] {
        assert!(!session::is_valid_id(id), "{id}");
    }
}
