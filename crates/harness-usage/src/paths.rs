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
