//! M3a: a child agent's transcript is its own session, linked to its parent's; the parent keeps
//! records of the spawn and the report that never join its conversation; and a build from before
//! M3a still reads both.

use std::path::Path;

use harness_core::{
    message::Message,
    session::{
        AgentRecord, AgentRecordKind, Entry, EntryKind, Lineage, Session, children_of, list,
    },
};

fn user(session: &mut Session, text: &str) -> String {
    session.append(EntryKind::Message {
        message: Message::User {
            content: text.into(),
        },
        display: None,
        note: false,
        plan: None,
        attribution: None,
        agent_record: None,
    })
}

fn lineage(parent: &str) -> Lineage {
    Lineage {
        parent: parent.into(),
        agent: "a1".into(),
        label: "explore".into(),
    }
}

fn record(kind: AgentRecordKind) -> EntryKind {
    EntryKind::Message {
        message: Message::User {
            content: "[agent a1 (explore)]".into(),
        },
        display: None,
        note: true,
        plan: None,
        attribution: None,
        agent_record: Some(AgentRecord {
            agent: "a1".into(),
            label: "explore".into(),
            kind,
            session: Some("child-session".into()),
            report: None,
        }),
    }
}

#[test]
fn a_child_session_names_its_parent_and_is_not_listed_for_resume() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent = Session::create(dir.path(), Path::new("/w"));
    user(&mut parent, "build it");
    let mut child = Session::create_child(dir.path(), Path::new("/w"), lineage(parent.id()));
    user(&mut child, "look around");
    assert_eq!(child.lineage().unwrap().parent, parent.id());
    assert!(parent.lineage().is_none());
    let listed: Vec<String> = list(dir.path()).into_iter().map(|s| s.id).collect();
    assert_eq!(listed, vec![parent.id().to_string()]);
    let kids = children_of(dir.path(), parent.id());
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, child.id());
}

#[test]
fn records_never_join_the_conversation_or_the_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent = Session::create(dir.path(), Path::new("/w"));
    let first = user(&mut parent, "build it");
    parent.append_aside(record(AgentRecordKind::Spawned));
    parent.append_aside(record(AgentRecordKind::Reported));
    assert_eq!(parent.leaf(), first);
    assert_eq!(parent.messages().len(), 1);
    let second = user(&mut parent, "and then");
    assert_eq!(
        parent.get(&second).unwrap().parent_id.as_deref(),
        Some(first.as_str())
    );
    let path = parent.path().unwrap().to_path_buf();
    drop(parent);
    let (reopened, warnings) = Session::open(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(reopened.leaf(), second);
    assert_eq!(reopened.messages().len(), 2);
}

#[test]
fn a_file_ending_in_a_record_reopens_on_the_last_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent = Session::create(dir.path(), Path::new("/w"));
    let first = user(&mut parent, "build it");
    parent.append_aside(record(AgentRecordKind::Reported));
    let path = parent.path().unwrap().to_path_buf();
    drop(parent);
    let (reopened, _) = Session::open(&path).unwrap();
    assert_eq!(reopened.leaf(), first);
}

/// The entry types as the last build before M3a wrote and read them (`git show 8542b7e:
/// crates/harness-core/src/session.rs`): none of M3a's fields, and unknown fields ignored.
mod m2 {
    use harness_core::{
        message::Message,
        role::{Handoff, Role, SwitchReason},
        session::RewindScope,
    };
    use serde::Deserialize;
    use std::path::PathBuf;

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    pub struct Entry {
        pub id: String,
        pub parent_id: Option<String>,
        #[serde(flatten)]
        pub kind: EntryKind,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum EntryKind {
        Session {
            version: u32,
            started_at: String,
            cwd: PathBuf,
        },
        Message {
            message: Message,
            #[serde(default)]
            display: Option<String>,
            #[serde(default)]
            note: bool,
            #[serde(default)]
            plan: Option<String>,
            #[serde(default)]
            attribution: Option<Attribution>,
        },
        Compaction {
            summary: String,
            first_kept: Option<String>,
        },
        Checkpoint {
            commit: String,
            #[serde(default)]
            workspace: Option<PathBuf>,
        },
        NoCheckpoint,
        Rewind {
            from: String,
            target: String,
            scope: RewindScope,
            snapshot: Option<String>,
        },
        UndoRewind {
            rewind: String,
            #[serde(default)]
            snapshot: Option<String>,
        },
        Version {
            version: u32,
        },
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize)]
    pub struct Attribution {
        pub role: Role,
        #[serde(default)]
        pub switch_reason: Option<SwitchReason>,
        #[serde(default)]
        pub handoff: Option<Handoff>,
    }
}

#[test]
fn a_build_from_before_m3a_reads_parent_and_child_files_without_skipping_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent = Session::create(dir.path(), Path::new("/w"));
    user(&mut parent, "build it");
    parent.append_aside(record(AgentRecordKind::Spawned));
    let mut child = Session::create_child(dir.path(), Path::new("/w"), lineage(parent.id()));
    user(&mut child, "look around");
    parent.append_aside(record(AgentRecordKind::Reported));
    user(&mut parent, "next");
    for session in [&parent, &child] {
        let text = std::fs::read_to_string(session.path().unwrap()).unwrap();
        for line in text.lines() {
            serde_json::from_str::<m2::Entry>(line)
                .unwrap_or_else(|e| panic!("an M2 build skips this line ({e}): {line}"));
        }
        let first: m2::Entry = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert!(matches!(
            first.kind,
            m2::EntryKind::Session { version: 2, .. }
        ));
    }
    // And the format the header claims is still 2.
    let text = std::fs::read_to_string(child.path().unwrap()).unwrap();
    let header: Entry = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert!(matches!(header.kind, EntryKind::Session { version: 2, .. }));
    let _ = (&parent, &child);
}

#[test]
fn an_m2_build_reads_a_record_that_ends_the_file_as_a_note_on_the_branch() {
    // Documented, not hidden: a build before M3a takes the last entry of a file as the leaf, so a
    // record flushed at the end of a turn is, to it, a note (`[agent a1 (x) reported]`) after the
    // conversation. It parses, it is a note, and nothing else changes.
    let dir = tempfile::tempdir().unwrap();
    let mut parent = Session::create(dir.path(), Path::new("/w"));
    user(&mut parent, "build it");
    parent.append_aside(record(AgentRecordKind::Reported));
    let text = std::fs::read_to_string(parent.path().unwrap()).unwrap();
    let m2_leaf: m2::Entry = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert!(
        matches!(m2_leaf.kind, m2::EntryKind::Message { note: true, .. }),
        "{m2_leaf:?}"
    );
}

/// M3c: the child agent's session on an assistant message (`attribution.delegate`) is a field an
/// M2 build does not know and so ignores: every line still reads as before.
#[test]
fn a_session_with_a_delegated_turn_reads_with_the_m2_entry_types() {
    use harness_core::{
        role::Role,
        session::{Attribution, DelegateMark},
    };
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::create(dir.path(), Path::new("/w"));
    user(&mut session, "fix it");
    session.append(EntryKind::Message {
        message: Message::Assistant {
            content: "done".into(),
            tool_calls: vec![],
            model: "claude-code/sonnet".into(),
        },
        display: None,
        note: false,
        plan: None,
        attribution: Some(Attribution {
            role: Role::Main,
            switch_reason: None,
            handoff: None,
            delegate: Some(Box::new(DelegateMark {
                model: "claude-code/sonnet".into(),
                session: "child-1".into(),
                cost_total_micros: Some(1_000),
            })),
        }),
        agent_record: None,
    });
    let text = std::fs::read_to_string(session.path().unwrap()).unwrap();
    assert!(text.contains("child-1"));
    for line in text.lines() {
        serde_json::from_str::<m2::Entry>(line)
            .unwrap_or_else(|e| panic!("an M2 build skips this line ({e}): {line}"));
    }
}
