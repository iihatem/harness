//! Sessions on disk. Each session is an append-only JSON Lines file of entries that form a tree:
//! every entry names its parent, and the active branch runs from the session's first line to the
//! current leaf. Rewinding moves the leaf, so earlier branches stay in the file.

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions, TryLockError},
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
    #[error("cannot lock {path}: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is not a harness session file")]
    NotASession(PathBuf),
    #[error(
        "{path} was saved by a newer version of harness (session format {version}); update harness to continue it"
    )]
    TooNew { path: PathBuf, version: u32 },
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
    /// Warnings about saving, until someone takes them.
    warnings: Vec<String>,
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
            warnings: Vec::new(),
        }
    }

    /// Opens a saved session to continue it, locking the file against other harness processes.
    /// Only a regular file named after the id on its first line is a session. Unreadable lines
    /// are skipped, and an incomplete last line (from a process that stopped while writing it)
    /// is removed from the file; the warnings say so, and what else is wrong with the file.
    pub fn open(path: &Path) -> Result<(Session, Vec<String>), SessionError> {
        let io = |source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        };
        let not_a_session = || SessionError::NotASession(path.to_path_buf());
        let file_id = file_id(path).ok_or_else(not_a_session)?;
        let mut file = open_regular(OpenOptions::new().read(true).append(true), path)
            .map_err(io)?
            .ok_or_else(not_a_session)?;
        let mut warnings = Vec::new();
        warnings.extend(lock(path, || file.try_lock())?);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
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
        // A last line without its newline is complete when it reads as an entry (an outside
        // edit), and was cut short otherwise.
        let tail = &bytes[complete..];
        let tail_entry = serde_json::from_slice::<Entry>(tail).ok();
        let tail_complete = tail_entry.is_some();
        entries.extend(tail_entry);
        // The id becomes part of other paths (the checkpoint index), so it must be the file's
        // own name, which was checked above.
        let Some(Entry {
            id,
            kind: EntryKind::Session { version, .. },
            ..
        }) = entries.first()
        else {
            return Err(not_a_session());
        };
        if id != file_id {
            return Err(not_a_session());
        }
        if *version > FORMAT_VERSION {
            return Err(SessionError::TooNew {
                path: path.to_path_buf(),
                version: *version,
            });
        }
        // Only a session in this format is changed.
        if tail_complete {
            file.write_all(b"\n").map_err(io)?;
        } else if !tail.is_empty() {
            warnings.push(format!(
                "the last line of {} was incomplete, probably because harness stopped while writing it; it was dropped",
                path.display()
            ));
            file.set_len(complete as u64).map_err(io)?;
        }
        if unreadable > 0 {
            warnings.push(format!(
                "skipped {unreadable} unreadable line(s) in {}",
                path.display()
            ));
        }
        let id = id.clone();
        let leaf = entries.last().map(|e| e.id.clone()).unwrap_or(id.clone());
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let written = entries.len();
        let session = Session {
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
            warnings: Vec::new(),
        };
        if let Some(problem) = session.walk().1 {
            warnings.push(format!("{}: {problem}", path.display()));
        }
        Ok((session, warnings))
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
        if let Err(e) = store.write(&self.entries, &mut self.warnings) {
            self.store = None;
            self.save_error = Some(e);
        }
    }

    /// Why the session stopped saving, the first time it is asked after that happened.
    pub fn take_save_error(&mut self) -> Option<std::io::Error> {
        self.save_error.take()
    }

    /// Warnings about saving the session (such as a file that cannot be locked), each once.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// The entries from the session's first line to the leaf.
    pub fn branch(&self) -> Vec<&Entry> {
        self.walk().0
    }

    /// The entries from the leaf back to the session's first line, in order, and what cut the
    /// walk short in a damaged file: a parent that is missing, or a loop.
    fn walk(&self) -> (Vec<&Entry>, Option<String>) {
        let mut out: Vec<&Entry> = Vec::new();
        let mut seen = HashSet::new();
        let mut problem = None;
        let mut next = Some(self.leaf.as_str());
        while let Some(id) = next {
            let Some(entry) = self.get(id) else {
                let child = out.last().map_or("?", |e| e.id.as_str());
                problem = Some(format!(
                    "the parent {id} of entry {child} is missing, so the conversation before it is not loaded"
                ));
                break;
            };
            if !seen.insert(id) {
                problem = Some(format!(
                    "the entries loop back to {id}, so the conversation before it is not loaded"
                ));
                break;
            }
            out.push(entry);
            next = entry.parent_id.as_deref();
        }
        out.reverse();
        (out, problem)
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
    /// Writes the entries not yet in the file; a warning about it goes to `warnings`.
    fn write(&mut self, entries: &[Entry], warnings: &mut Vec<String>) -> std::io::Result<()> {
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
            warnings.extend(
                lock(&self.path, || file.try_lock())
                    .map_err(|e| std::io::Error::other(e.to_string()))?,
            );
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
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
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
    let file = open_regular(OpenOptions::new().read(true), path).ok()??;
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

/// Opens `path` with `options` when it is a regular file, never following a symbolic link in
/// its last component nor waiting on a FIFO; `None` when it is something else.
fn open_regular(options: &mut OpenOptions, path: &Path) -> std::io::Result<Option<File>> {
    let file = match options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(file.metadata()?.is_file().then_some(file))
}

/// Locks the session file at `path` with `try_lock`, against other harness processes. A lock
/// can outlive its owner for a moment while that process starts a child (the child holds a copy
/// of the descriptor until it runs its program), so a held lock is tried a few more times before
/// the session counts as in use. Where the file system cannot lock files, the session goes on
/// unlocked, and the returned warning says so.
fn lock(
    path: &Path,
    mut try_lock: impl FnMut() -> Result<(), TryLockError>,
) -> Result<Option<String>, SessionError> {
    let mut attempts = 0;
    loop {
        match try_lock() {
            Ok(()) => return Ok(None),
            Err(TryLockError::WouldBlock) if attempts < 10 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(TryLockError::WouldBlock) => return Err(SessionError::InUse(path.to_path_buf())),
            Err(TryLockError::Error(e)) if locks_unsupported(&e) => {
                return Ok(Some(format!(
                    "cannot lock {} ({e}): its file system does not support file locks, so nothing stops another harness process from writing to this session at the same time",
                    path.display()
                )));
            }
            Err(TryLockError::Error(source)) => {
                return Err(SessionError::Lock {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
}

/// Whether `error` says the file system cannot lock files (some network and FUSE mounts).
fn locks_unsupported(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Unsupported
        || error.raw_os_error().is_some_and(|code| {
            [libc::ENOLCK, libc::ENOTSUP, libc::EOPNOTSUPP, libc::ENOSYS].contains(&code)
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

/// A session id that sorts by start time: `20260927T123456Z-1a2b3c4d`.
fn new_session_id() -> String {
    let stamp: String = time::timestamp(time::now_unix())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    format!("{stamp}-{:08x}", random_u64() as u32)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn failing(error: std::io::Error) -> impl FnMut() -> Result<(), TryLockError> {
        let mut error = Some(error);
        move || Err(TryLockError::Error(error.take().expect("tried once")))
    }

    // Review D M2: some network and FUSE file systems cannot lock files at all. Sessions there
    // work unlocked, with a warning that says it is about locking.
    #[test]
    fn a_file_system_without_locks_is_used_unlocked_with_a_warning() {
        let path = Path::new("/mnt/nfs/s.jsonl");
        for error in [
            std::io::Error::from(std::io::ErrorKind::Unsupported),
            std::io::Error::from_raw_os_error(libc::ENOLCK),
            std::io::Error::from_raw_os_error(libc::ENOTSUP),
        ] {
            let warning = lock(path, failing(error)).unwrap().expect("a warning");
            assert!(
                warning.starts_with("cannot lock /mnt/nfs/s.jsonl")
                    && warning.contains("file locks"),
                "{warning}"
            );
        }
    }

    #[test]
    fn other_lock_errors_are_named_as_such() {
        let path = Path::new("/s.jsonl");
        let error = lock(
            path,
            failing(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        )
        .unwrap_err();
        assert!(matches!(error, SessionError::Lock { .. }), "{error:?}");
        assert!(
            error.to_string().starts_with("cannot lock /s.jsonl"),
            "{error}"
        );
        assert!(matches!(
            lock(path, || Err(TryLockError::WouldBlock)),
            Err(SessionError::InUse(_))
        ));
        assert_eq!(lock(path, || Ok(())).unwrap(), None);
    }
}
