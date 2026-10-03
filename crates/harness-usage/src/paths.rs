//! Where usage data lives, under harness's data directory.

use std::path::{Path, PathBuf};

/// The directories usage data is kept in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// The ledger, its report cache and the window snapshots: `<data>/usage`.
    pub usage: PathBuf,
    /// The outcome log: `<data>/outcomes`.
    pub outcomes: PathBuf,
    /// The downloaded price table: `<data>/pricing.json`.
    pub pricing: PathBuf,
}

impl Dirs {
    /// The directories under harness's data directory `data`.
    pub fn under(data: &Path) -> Dirs {
        Dirs {
            usage: data.join("usage"),
            outcomes: data.join("outcomes"),
            pricing: data.join("pricing.json"),
        }
    }
}

/// Creates `dir` and its parents, private to the user (0700).
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Appends `text` (whole lines) to `path`, creating it private (0600). When the file ends
/// without a newline (a crash cut its last line short), a newline goes first, so the torn line
/// stays one bad line and the new ones are not glued to it. One `write` of the whole text.
pub(crate) fn append_lines(path: &Path, text: &str) -> std::io::Result<()> {
    use std::{io::Write, os::unix::fs::FileExt, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let len = file.metadata()?.len();
    let mut line = String::with_capacity(text.len() + 1);
    if len > 0 {
        let mut last = [0u8; 1];
        if file.read_at(&mut last, len - 1)? == 1 && last[0] != b'\n' {
            line.push('\n');
        }
    }
    line.push_str(text);
    file.write_all(line.as_bytes())
}
