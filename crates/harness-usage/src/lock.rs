//! The advisory lock on a usage directory (`usage/`, `outcomes/`), kept in a `.lock` file in it.
//!
//! Every append takes it shared, so appends from any number of processes go on together; `forget`,
//! the only thing that rewrites or deletes a usage file, takes it exclusive, so an append made
//! while it runs waits for it instead of being lost to the rewrite. The lock is released when the
//! guard is dropped, or when the process ends.

use std::{
    fs::File,
    path::{Path, PathBuf},
};

/// The name of the lock file in a usage directory: not a ledger, window or outcome file, so
/// nothing reads or deletes it as one.
pub const FILE: &str = ".lock";

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub struct DirLock {
    file: File,
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn open(dir: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    crate::paths::create_private_dir(dir)?;
    let path: PathBuf = dir.join(FILE);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
}

/// Waits for and takes the lock on `dir` shared (creating the directory, 0700, when needed).
pub fn shared(dir: &Path) -> std::io::Result<DirLock> {
    let file = open(dir)?;
    file.lock_shared()?;
    Ok(DirLock { file })
}

/// Waits for and takes the lock on `dir` exclusive (creating the directory, 0700, when needed).
pub fn exclusive(dir: &Path) -> std::io::Result<DirLock> {
    let file = open(dir)?;
    file.lock()?;
    Ok(DirLock { file })
}
