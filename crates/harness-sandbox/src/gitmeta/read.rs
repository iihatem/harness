//! Reads the small files git keeps its settings in (gitfiles, `commondir`,
//! `.gitignore`, `info/exclude`) from a workspace a sandboxed command can
//! write. Whatever it planted there, the read never blocks, never follows a
//! symlink in the last component, and stops at a size limit.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Reads at most `limit` bytes of the regular file at `path`.
///
/// Anything else is refused: a symlink (git does not follow a symlinked
/// `.gitignore` either), a FIFO, a device, a directory, a socket. It is
/// never opened when it is seen for what it is first; the file is then
/// opened with `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY` and checked again
/// through its descriptor, so one swapped in after the first check is
/// neither followed nor waited on, nor read.
///
/// A missing file is `NotFound`, or `NotADirectory` when a component is not
/// a directory; any other error means the file exists and was not read.
pub(super) fn read_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(not_regular());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `err`, from [`read_regular`], means there is no such file.
pub(super) fn missing(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// [`read_regular`], which must return within 10 seconds: a FIFO that is
    /// opened for reading blocks until someone writes to it.
    fn read(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
        let path = path.to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(read_regular(&path, limit)));
        rx.recv_timeout(Duration::from_secs(10))
            .expect("read_regular blocked")
    }

    fn dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    #[test]
    fn a_regular_file_is_read_up_to_the_limit() {
        let (_d, dir) = dir();
        std::fs::write(dir.join("file"), "abc").unwrap();
        assert_eq!(read(&dir.join("file"), 10).unwrap(), b"abc");
        assert_eq!(read(&dir.join("file"), 2).unwrap(), b"ab");
    }

    #[test]
    fn a_missing_file_is_not_found() {
        let (_d, dir) = dir();
        let err = read(&dir.join("missing"), 10).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        std::fs::write(dir.join("file"), "abc").unwrap();
        let err = read(&dir.join("file/below"), 10).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotADirectory);
    }

    #[test]
    fn anything_but_a_regular_file_is_refused() {
        let (_d, dir) = dir();
        std::fs::write(dir.join("file"), "abc").unwrap();
        symlink("file", dir.join("link")).unwrap();
        symlink("/dev/zero", dir.join("zero")).unwrap();
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(dir.join("fifo"))
            .status()
            .unwrap();
        assert!(status.success());
        for path in [
            dir.join("link"),
            dir.join("zero"),
            dir.join("fifo"),
            dir.clone(),
            PathBuf::from("/dev/null"),
        ] {
            let err = read(&path, 10).unwrap_err();
            assert!(!missing(&err), "{path:?}: {err}");
        }
    }
}
