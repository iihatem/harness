//! Reads instruction and command files, which a cloned repository controls. Whatever it planted,
//! the read never follows a symlink in the last component, never blocks, and stops at a limit.
//! It mirrors `harness-sandbox`'s `gitmeta::read::read_regular`, which this crate cannot depend on.

use std::{
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

/// Reads at most `limit` bytes of the regular file at `path`.
///
/// Callers pass a path already resolved through symlinks, where their policy allows links, so a
/// symlink here was swapped in after that. Anything but a regular file is refused (see
/// [`not_regular`]): a symlink, a FIFO, a device, a directory, a socket. It is never opened when
/// it is seen for what it is first; the file is then opened with `O_NOFOLLOW | O_NONBLOCK |
/// O_NOCTTY` and checked again through its descriptor, so one swapped in after the first check is
/// neither followed nor waited on, nor read.
pub(crate) fn read_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(refused());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                refused()
            } else {
                e
            }
        })?;
    if !file.metadata()?.is_file() {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `err`, from [`read_regular`], means the path is not a regular file.
pub(crate) fn not_regular(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::InvalidInput
}

fn refused() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::{ffi::OsStrExt, fs::symlink},
        path::PathBuf,
        sync::mpsc,
        time::Duration,
    };

    use super::*;

    /// [`read_regular`], which must return within 10 seconds: a FIFO opened for reading blocks
    /// until someone writes to it.
    fn read(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
        let path = path.to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || tx.send(read_regular(&path, limit)));
        rx.recv_timeout(Duration::from_secs(10))
            .expect("read_regular blocked")
    }

    #[test]
    fn a_regular_file_is_read_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(read(&file, 10).unwrap(), b"abc");
        assert_eq!(read(&file, 2).unwrap(), b"ab");
    }

    #[test]
    fn anything_but_a_regular_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        std::fs::write(dir.join("file"), "abc").unwrap();
        symlink("file", dir.join("link")).unwrap();
        symlink("/dev/zero", dir.join("zero")).unwrap();
        let fifo = std::ffi::CString::new(dir.join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        for path in [
            dir.join("link"),
            dir.join("zero"),
            dir.join("fifo"),
            dir.clone(),
            PathBuf::from("/dev/null"),
        ] {
            let err = read(&path, 10).unwrap_err();
            assert!(not_regular(&err), "{path:?}: {err}");
        }
        let err = read(&dir.join("missing"), 10).unwrap_err();
        assert!(!not_regular(&err), "{err}");
    }
}
