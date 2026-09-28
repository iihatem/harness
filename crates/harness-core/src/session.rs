//! Sessions on disk. Each session is an append-only JSON Lines file of entries that form a tree:
//! every entry names its parent, and the active branch runs from the session's first line to the
//! current leaf. Rewinding moves the leaf, so earlier branches stay in the file.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    hash::{BuildHasher, Hasher},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::{compaction, message::Message, time};

/// The session file format written by this version.
pub const FORMAT_VERSION: u32 = 1;

/// One line of a session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub parent_id: Option<String>,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    /// The first line, for the session itself. Every other entry descends from it.
    Session {
        version: u32,
        started_at: String,
        cwd: PathBuf,
    },
    Message {
        message: Message,
        /// What the user typed, when it differs from the message (a slash command).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<String>,
        /// A note from harness, such as a mode change, rather than something the user typed.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        note: bool,
    },
    /// A summary that replaces the conversation before `first_kept` (all of it when `None`) on
    /// this branch. The summarized entries stay in the file.
    Compaction {
        summary: String,
        first_kept: Option<String>,
    },
    /// A snapshot of the workspace taken before the turn's first change.
    Checkpoint { commit: String },
    /// A rewind to just before the user message `target`. The active branch continues from this
    /// entry's parent. `from` is the leaf before the rewind, and `snapshot` the workspace just
    /// before files were restored, so the rewind can be undone.
    Rewind {
        from: String,
        target: String,
        scope: RewindScope,
        snapshot: Option<String>,
    },
    /// Undoes the rewind `rewind`: the active branch continues from where that rewind started.
    UndoRewind { rewind: String },
}

/// What a rewind restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindScope {
    CodeAndConversation,
    Code,
    Conversation,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("cannot open {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is open in another harness process")]
    InUse(PathBuf),
    #[error("{0} is not a harness session file")]
    NotASession(PathBuf),
}

/// A session: its entries, the current leaf, and (unless it lives only in memory) its file.
#[derive(Debug)]
pub struct Session {
    id: String,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    leaf: String,
    store: Option<Store>,
    /// Why the session stopped saving, until someone takes it.
    save_error: Option<std::io::Error>,
}

/// Where a session is saved. The file is created, and locked, when the first entry after the
/// header is appended, so a session nobody spoke in leaves no file behind.
#[derive(Debug)]
struct Store {
    path: PathBuf,
    file: Option<File>,
    /// How many entries are already in the file.
    written: usize,
}

impl Session {
    /// A session that is never saved.
    pub fn in_memory(cwd: &Path) -> Session {
        Session::with_header(new_session_id(), cwd, None)
    }

    /// A new session saved as `<dir>/<id>.jsonl`.
    pub fn create(dir: &Path, cwd: &Path) -> Session {
        let id = new_session_id();
        let path = dir.join(format!("{id}.jsonl"));
        Session::with_header(
            id,
            cwd,
            Some(Store {
                path,
                file: None,
                written: 0,
            }),
        )
    }

    fn with_header(id: String, cwd: &Path, store: Option<Store>) -> Session {
        let header = Entry {
            id: id.clone(),
            parent_id: None,
            kind: EntryKind::Session {
                version: FORMAT_VERSION,
                started_at: time::timestamp(time::now_unix()),
                cwd: cwd.to_path_buf(),
            },
        };
        Session {
            index: HashMap::from([(id.clone(), 0)]),
            leaf: id.clone(),
            id,
            entries: vec![header],
            store,
            save_error: None,
        }
    }

    /// Opens a saved session to continue it, locking the file against other harness processes.
    /// Unreadable lines are skipped, and an incomplete last line (from a process that stopped
    /// while writing it) is removed from the file; the warnings say so.
    pub fn open(path: &Path) -> Result<(Session, Vec<String>), SessionError> {
        let io = |source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        };
        let not_a_session = || SessionError::NotASession(path.to_path_buf());
        let file_id = file_id(path).ok_or_else(not_a_session)?;
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(io)?;
        // A lock can outlive its owner for a moment while this process starts a child (the child
        // holds a copy of the descriptor until it runs its program), so a held lock is tried a
        // few more times before the session counts as in use.
        let mut attempts = 0;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if attempts < 10 => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(SessionError::InUse(path.to_path_buf()));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(io(e)),
            }
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        let mut warnings = Vec::new();
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if complete < bytes.len() {
            warnings.push(format!(
                "the last line of {} was incomplete, probably because harness stopped while writing it; it was dropped",
                path.display()
            ));
            file.set_len(complete as u64).map_err(io)?;
        }
        let mut entries: Vec<Entry> = Vec::new();
        let mut unreadable = 0;
        for line in bytes[..complete].split(|b| *b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Entry>(line) {
                Ok(entry) => entries.push(entry),
                Err(_) => unreadable += 1,
            }
        }
        if unreadable > 0 {
            warnings.push(format!(
                "skipped {unreadable} unreadable line(s) in {}",
                path.display()
            ));
        }
        // The id becomes part of other paths (the checkpoint index), so it must be the file's
        // own name, which was checked above.
        let Some(Entry {
            id,
            kind: EntryKind::Session { .. },
            ..
        }) = entries.first()
        else {
            return Err(not_a_session());
        };
        if id != file_id {
            return Err(not_a_session());
        }
        let id = id.clone();
        let leaf = entries.last().map(|e| e.id.clone()).unwrap_or(id.clone());
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let written = entries.len();
        Ok((
            Session {
                id,
                entries,
                index,
                leaf,
                store: Some(Store {
                    path: path.to_path_buf(),
                    file: Some(file),
                    written,
                }),
                save_error: None,
            },
            warnings,
        ))
    }

    /// The session's id, which [`is_valid_id`] accepts.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session's file, unless it lives only in memory.
    pub fn path(&self) -> Option<&Path> {
        self.store.as_ref().map(|s| s.path.as_path())
    }

    /// The entry new entries are appended under.
    pub fn leaf(&self) -> &str {
        &self.leaf
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    /// Appends `kind` under the current leaf, which it becomes. Returns its id.
    pub fn append(&mut self, kind: EntryKind) -> String {
        let parent = self.leaf.clone();
        self.append_under(&parent, kind)
    }

    /// Appends `kind` under `parent` (an existing entry), and makes it the leaf. Returns its id.
    /// The entry is written to the file at once; if that fails, the session keeps working in
    /// memory and [`take_save_error`](Self::take_save_error) says why.
    pub fn append_under(&mut self, parent: &str, kind: EntryKind) -> String {
        assert!(self.index.contains_key(parent), "unknown parent {parent}");
        let id = loop {
            let candidate = format!("{:08x}", random_u64() as u32);
            if !self.index.contains_key(&candidate) {
                break candidate;
            }
        };
        self.index.insert(id.clone(), self.entries.len());
        self.entries.push(Entry {
            id: id.clone(),
            parent_id: Some(parent.to_string()),
            kind,
        });
        self.leaf = id.clone();
        self.save();
        id
    }

    /// Writes the entries not yet in the file, creating and locking it first if needed. On
    /// failure the session stops saving, so later entries are kept in memory only.
    fn save(&mut self) {
        let Some(store) = self.store.as_mut() else {
            return;
        };
        if let Err(e) = store.write(&self.entries) {
            self.store = None;
            self.save_error = Some(e);
        }
    }

    /// Why the session stopped saving, the first time it is asked after that happened.
    pub fn take_save_error(&mut self) -> Option<std::io::Error> {
        self.save_error.take()
    }

    /// The entries from the session's first line to the leaf.
    pub fn branch(&self) -> Vec<&Entry> {
        let mut out = Vec::new();
        let mut next = Some(self.leaf.as_str());
        while let Some(id) = next {
            let Some(entry) = self.get(id) else { break };
            out.push(entry);
            if out.len() > self.entries.len() {
                break;
            }
            next = entry.parent_id.as_deref();
        }
        out.reverse();
        out
    }

    /// The conversation on the active branch, with each message's entry id. A compaction
    /// replaces the messages before its first kept one with its summary.
    pub fn messages(&self) -> Vec<(String, Message)> {
        let mut out = Vec::new();
        for entry in self.branch() {
            match &entry.kind {
                EntryKind::Message { message, .. } => {
                    out.push((entry.id.clone(), message.clone()));
                }
                EntryKind::Compaction {
                    summary,
                    first_kept,
                } => {
                    let from = first_kept
                        .as_ref()
                        .and_then(|id| out.iter().position(|(entry, _)| entry == id))
                        .unwrap_or(out.len());
                    let kept = out.split_off(from);
                    out = vec![(entry.id.clone(), compaction::summary_message(summary))];
                    out.extend(kept);
                }
                _ => {}
            }
        }
        out
    }
}

impl Store {
    fn write(&mut self, entries: &[Entry]) -> std::io::Result<()> {
        if self.file.is_none() {
            // Sessions hold prompts, code and tool output: only their owner may read them.
            if let Some(dir) = self.path.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)?;
            }
            let file = OpenOptions::new()
                .append(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.path)?;
            file.try_lock().map_err(std::io::Error::from)?;
            self.file = Some(file);
        }
        let file = self.file.as_mut().expect("opened above");
        let mut text = String::new();
        for entry in &entries[self.written..] {
            text.push_str(&serde_json::to_string(entry).map_err(std::io::Error::other)?);
            text.push('\n');
        }
        // One write per batch, so a crash leaves at most one incomplete line.
        file.write_all(text.as_bytes())?;
        self.written = entries.len();
        Ok(())
    }
}

/// A session in a project's sessions directory, as listed for `--resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub started_at: String,
    /// The first thing the user typed.
    pub first_message: Option<String>,
    pub modified: SystemTime,
}

/// The sessions in `dir`, most recently used first.
pub fn list(dir: &Path) -> Vec<SessionSummary> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionSummary> = read
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|path| summary(&path))
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.id.cmp(&a.id)));
    out
}

/// What `--resume` lists about the session file at `path`; `None` when it is not one that
/// [`Session::open`] would open.
fn summary(path: &Path) -> Option<SessionSummary> {
    let id = file_id(path)?;
    let file = File::open(path).ok()?;
    let modified = file.metadata().ok()?.modified().ok()?;
    let mut lines = BufReader::new(file).lines();
    let header: Entry = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    let EntryKind::Session { started_at, .. } = header.kind else {
        return None;
    };
    if header.id != id {
        return None;
    }
    let first_message =
        lines.map_while(Result::ok).find_map(|line| {
            match serde_json::from_str::<Entry>(&line).ok()?.kind {
                EntryKind::Message {
                    message: Message::User { content },
                    display,
                    note: false,
                } => Some(display.unwrap_or(content)),
                _ => None,
            }
        });
    Some(SessionSummary {
        id: id.to_string(),
        path: path.to_path_buf(),
        started_at,
        first_message,
        modified,
    })
}

/// The session id a file at `path` must have: its name without `.jsonl`, when that is a valid
/// id.
fn file_id(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    is_valid_id(name).then_some(name)
}

/// Whether `id` can be a session id: 1 to 64 ASCII letters, digits and dashes. Session ids
/// become file names and parts of other paths, so nothing else is accepted.
pub fn is_valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A session id that sorts by start time: `20260927T123456Z-1a2b`.
fn new_session_id() -> String {
    let stamp: String = time::timestamp(time::now_unix())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("{stamp}-{:04x}", random_u64() as u16)
}

/// A random number from the standard library's per-process hash keys.
fn random_u64() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish()
}
