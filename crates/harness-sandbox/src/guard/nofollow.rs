//! Filesystem operations in a workspace a sandboxed command can change at
//! any moment, none of which follows a symlink in any component of a path.
//!
//! Every path is reached from a trusted anchor directory (the workspace) one
//! directory descriptor at a time: on Linux with `openat2` and
//! `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`; elsewhere (macOS, or where
//! `openat2` is missing or refused) component by component with
//! `openat(O_DIRECTORY | O_NOFOLLOW)`. The operation itself is one `*at` call
//! on a single name, relative to its parent's descriptor. A directory
//! swapped for a symlink on the way makes the operation fail (`ELOOP` or
//! `ENOTDIR`) instead of acting where the symlink leads.
//!
//! A descriptor keeps pointing at the directory it opened when that
//! directory is renamed later. Wherever a command moved it, it is somewhere
//! that command could write itself, so acting there reaches nothing the
//! command could not.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

/// How a directory is opened: never through a symlink, and never waiting on
/// a FIFO swapped in for it.
const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;

/// The longest symlink target read.
const MAX_LINK_BYTES: usize = 1 << 16;

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Dir,
    File,
    Symlink,
    /// A FIFO, socket or device node.
    Other,
}

/// An entry's `lstat`, as far as the guard uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub(crate) kind: Kind,
    /// The permission bits (`0o7777`).
    pub(crate) mode: u32,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) nlink: u64,
    pub(crate) size: u64,
    pub(crate) mtime: (i64, i64),
    pub(crate) ctime: (i64, i64),
}

impl Stat {
    /// Whether `other` is the same entry: the same type, device and inode.
    pub(crate) fn same_entry(&self, other: &Stat) -> bool {
        self.kind == other.kind && self.dev == other.dev && self.ino == other.ino
    }

    /// Whether `other` is the same entry, not written to since: the same
    /// size and times too.
    pub(crate) fn unchanged(&self, other: &Stat) -> bool {
        self.same_entry(other)
            && self.size == other.size
            && self.mtime == other.mtime
            && self.ctime == other.ctime
    }

    #[allow(
        clippy::unnecessary_cast,
        clippy::useless_conversion,
        reason = "the field types of `stat` differ between Linux and macOS"
    )]
    fn from_raw(st: &libc::stat) -> Stat {
        let mode = st.st_mode as u32;
        let kind = match mode & (libc::S_IFMT as u32) {
            m if m == libc::S_IFDIR as u32 => Kind::Dir,
            m if m == libc::S_IFREG as u32 => Kind::File,
            m if m == libc::S_IFLNK as u32 => Kind::Symlink,
            _ => Kind::Other,
        };
        Stat {
            kind,
            mode: mode & 0o7777,
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
            nlink: st.st_nlink as u64,
            size: st.st_size as u64,
            mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
            ctime: (st.st_ctime as i64, st.st_ctime_nsec as i64),
        }
    }
}

/// An open directory.
#[derive(Debug)]
pub(crate) struct Dir {
    fd: OwnedFd,
}

impl Dir {
    /// Opens the trusted directory `path`. Its last component must not be a
    /// symlink; the ones before it are followed.
    pub(crate) fn open(path: &Path) -> io::Result<Dir> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let fd = cvt(unsafe { libc::open(path.as_ptr(), DIR_FLAGS) })?;
        // SAFETY: `open` just returned this descriptor, which nothing else owns.
        Ok(Dir {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Another descriptor for this directory.
    pub(crate) fn try_clone(&self) -> io::Result<Dir> {
        Ok(Dir {
            fd: self.fd.try_clone()?,
        })
    }

    /// The directory at `rel` below this one (this one again when `rel` is
    /// empty). Every component of `rel` must be a plain name, and none may be
    /// a symlink.
    pub(crate) fn beneath(&self, rel: &Path) -> io::Result<Dir> {
        let names = plain_names(rel)?;
        if names.is_empty() {
            return self.try_clone();
        }
        #[cfg(target_os = "linux")]
        if let Some(found) = openat2_beneath(self, rel) {
            return found;
        }
        walk_beneath(self, &names)
    }

    /// The directory `name` in this one, when it is not a symlink.
    pub(crate) fn open_dir(&self, name: &OsStr) -> io::Result<Dir> {
        Ok(Dir {
            fd: self.open_at(name, DIR_FLAGS, 0)?,
        })
    }

    /// This directory's `fstat`.
    pub(crate) fn stat_self(&self) -> io::Result<Stat> {
        fstat(self.fd.as_raw_fd())
    }

    /// The entry `name`'s `lstat`.
    pub(crate) fn stat(&self, name: &OsStr) -> io::Result<Stat> {
        let name = plain_name(name)?;
        let mut st = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `name` is NUL-terminated and `st` is large enough; on
        // success `fstatat` has initialized it.
        cvt(unsafe {
            libc::fstatat(
                self.raw(),
                name.as_ptr(),
                st.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        // SAFETY: initialized by the successful call above.
        Ok(Stat::from_raw(unsafe { st.assume_init_ref() }))
    }

    /// The names in this directory, without `.` and `..`.
    pub(crate) fn entries(&self) -> io::Result<Vec<OsString>> {
        // `fdopendir` takes the descriptor over, so give it a copy. The copy
        // shares the read offset, hence the rewind.
        let copy = self.fd.try_clone()?.into_raw_fd();
        // SAFETY: `copy` is an open directory descriptor nothing else owns.
        let stream = unsafe { libc::fdopendir(copy) };
        if stream.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: `fdopendir` failed, so `copy` is still ours to close.
            unsafe { libc::close(copy) };
            return Err(err);
        }
        let stream = Stream(stream);
        // SAFETY: `stream.0` is a valid, open directory stream.
        unsafe { libc::rewinddir(stream.0) };
        let mut names = Vec::new();
        loop {
            set_errno(0);
            // SAFETY: `stream.0` is a valid, open directory stream.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(0) {
                    return Ok(names);
                }
                return Err(err);
            }
            // SAFETY: `readdir` returned a valid entry, whose `d_name` is
            // NUL-terminated and lives until the next call on the stream.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsStr::from_bytes(name).to_os_string());
            }
        }
    }

    /// Opens the regular file `name` for reading, and its `fstat`. Anything
    /// else (a symlink, a FIFO, a device) is refused without being opened,
    /// or, when it was swapped in after it was looked at, without blocking.
    pub(crate) fn open_regular(&self, name: &OsStr) -> io::Result<(File, Stat)> {
        let seen = self.stat(name)?;
        if seen.kind != Kind::File {
            return Err(not_regular());
        }
        let flags =
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC;
        let file = File::from(self.open_at(name, flags, 0)?);
        let opened = stat_file(&file)?;
        if !opened.same_entry(&seen) {
            return Err(not_regular());
        }
        Ok((file, opened))
    }

    /// Creates the file `name` with `mode`, failing if anything (a symlink
    /// included) is there already.
    pub(crate) fn create_file(&self, name: &OsStr, mode: u32) -> io::Result<File> {
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        Ok(File::from(self.open_at(name, flags, mode)?))
    }

    /// Creates the directory `name` with `mode` (less the umask).
    pub(crate) fn mkdir(&self, name: &OsStr, mode: u32) -> io::Result<()> {
        let name = plain_name(name)?;
        // SAFETY: `name` is NUL-terminated and outlives the call.
        cvt(unsafe { libc::mkdirat(self.raw(), name.as_ptr(), mode as libc::mode_t) }).map(drop)
    }

    /// Creates the symlink `name` to `target`.
    pub(crate) fn symlink(&self, target: &Path, name: &OsStr) -> io::Result<()> {
        let target = CString::new(target.as_os_str().as_bytes())?;
        let name = plain_name(name)?;
        // SAFETY: both strings are NUL-terminated and outlive the call.
        cvt(unsafe { libc::symlinkat(target.as_ptr(), self.raw(), name.as_ptr()) }).map(drop)
    }

    /// Where the symlink `name` points.
    pub(crate) fn read_link(&self, name: &OsStr) -> io::Result<PathBuf> {
        let name = plain_name(name)?;
        let mut buf = vec![0u8; 256];
        loop {
            // SAFETY: `name` is NUL-terminated and `buf` has `buf.len()`
            // writable bytes.
            let read = unsafe {
                libc::readlinkat(
                    self.raw(),
                    name.as_ptr(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            let read = usize::try_from(read).map_err(|_| io::Error::last_os_error())?;
            if read < buf.len() {
                buf.truncate(read);
                return Ok(PathBuf::from(OsString::from_vec(buf)));
            }
            if buf.len() >= MAX_LINK_BYTES {
                return Err(io::Error::other("the symlink's target is too long"));
            }
            buf.resize(buf.len() * 2, 0);
        }
    }

    /// Renames `name` to `to_name` in `to`, failing with `AlreadyExists` if
    /// anything is there: it never replaces an entry.
    pub(crate) fn rename_new(&self, name: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
        let from = plain_name(name)?;
        let dest = plain_name(to_name)?;
        match rename_exclusive(self.raw(), &from, to.raw(), &dest) {
            Some(renamed) => renamed,
            None => self.rename_looking_first(name, to, to_name),
        }
    }

    /// [`rename_new`](Self::rename_new) where the system or filesystem has
    /// no exclusive rename: looks, then renames. Something put at `to_name`
    /// between the two would be replaced. For a file or symlink that window
    /// is accepted; a directory could replace an empty directory, so it is
    /// refused.
    fn rename_looking_first(&self, name: &OsStr, to: &Dir, to_name: &OsStr) -> io::Result<()> {
        let from = plain_name(name)?;
        let dest = plain_name(to_name)?;
        if self.stat(name)?.kind == Kind::Dir {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this filesystem cannot rename a directory without the risk of replacing one",
            ));
        }
        match to.stat(to_name) {
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        // SAFETY: both names are NUL-terminated and outlive the call.
        cvt(unsafe { libc::renameat(self.raw(), from.as_ptr(), to.raw(), dest.as_ptr()) }).map(drop)
    }

    /// Removes the entry `name`: an empty directory with `dir`, anything but
    /// a directory without it.
    pub(crate) fn remove(&self, name: &OsStr, dir: bool) -> io::Result<()> {
        let name = plain_name(name)?;
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is NUL-terminated and outlives the call.
        cvt(unsafe { libc::unlinkat(self.raw(), name.as_ptr(), flags) }).map(drop)
    }

    /// Sets the permission bits of the entry `name`, never through a symlink.
    pub(crate) fn chmod(&self, name: &OsStr, mode: u32) -> io::Result<()> {
        let cname = plain_name(name)?;
        // SAFETY: `cname` is NUL-terminated and outlives the call.
        let changed = unsafe {
            libc::fchmodat(
                self.raw(),
                cname.as_ptr(),
                mode as libc::mode_t,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if changed == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        let unsupported = err
            .raw_os_error()
            .is_some_and(|code| code == libc::EOPNOTSUPP || code == libc::ENOTSUP);
        if !unsupported {
            return Err(err);
        }
        // A C library without a no-follow `chmod` (or a symlink): through a
        // descriptor, which only a directory or regular file gets.
        match self.stat(name)?.kind {
            Kind::Dir => self.open_dir(name)?.set_mode(mode),
            Kind::File => self
                .open_regular(name)?
                .0
                .set_permissions(PermissionsExt::from_mode(mode)),
            Kind::Symlink | Kind::Other => Err(err),
        }
    }

    /// Sets this directory's permission bits.
    pub(crate) fn set_mode(&self, mode: u32) -> io::Result<()> {
        // SAFETY: plain call on a descriptor this `Dir` owns.
        cvt(unsafe { libc::fchmod(self.raw(), mode as libc::mode_t) }).map(drop)
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// `openat` on the single name `name`.
    fn open_at(&self, name: &OsStr, flags: libc::c_int, mode: u32) -> io::Result<OwnedFd> {
        let name = plain_name(name)?;
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let fd =
            cvt(unsafe { libc::openat(self.raw(), name.as_ptr(), flags, mode as libc::c_uint) })?;
        // SAFETY: `openat` just returned this descriptor, which nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

/// A directory stream, closed on drop.
struct Stream(*mut libc::DIR);

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream is open, and closed only here.
        unsafe { libc::closedir(self.0) };
    }
}

/// Opens the directory at the absolute `path`, which was canonical when
/// harness started, creating it and its missing parents with `mode`. No
/// symlink is followed in any component: one swapped in since makes it fail.
pub(crate) fn create_dirs(path: &Path, mode: u32) -> io::Result<Dir> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(invalid("not an absolute path"));
    }
    let names: Vec<&OsStr> = components
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _ => Err(invalid("not a path of plain names")),
        })
        .collect::<io::Result<_>>()?;
    let mut dir = Dir::open(Path::new("/"))?;
    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == names.len();
        let next = if last {
            dir.open_dir(name)
        } else {
            dir.pass_through(name)
        };
        dir = match next {
            Ok(next) => next,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let created = match dir.mkdir(name, mode) {
                    Ok(()) => true,
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
                    Err(err) => return Err(err),
                };
                let made = dir.open_dir(name)?;
                if created {
                    made.set_mode(mode)?;
                }
                made
            }
            Err(err) => return Err(err),
        };
    }
    Ok(dir)
}

impl Dir {
    /// The directory `name` in this one, when it is not a symlink, opened
    /// only to reach what is below it: on Linux with `O_PATH`, which needs
    /// no read permission on it.
    fn pass_through(&self, name: &OsStr) -> io::Result<Dir> {
        #[cfg(target_os = "linux")]
        {
            let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            Ok(Dir {
                fd: self.open_at(name, flags, 0)?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        self.open_dir(name)
    }
}

/// [`Dir::beneath`] one component at a time, with `openat(O_NOFOLLOW)`.
fn walk_beneath(dir: &Dir, names: &[&OsStr]) -> io::Result<Dir> {
    let Some((first, rest)) = names.split_first() else {
        return dir.try_clone();
    };
    let mut current = dir.open_dir(first)?;
    for name in rest {
        current = current.open_dir(name)?;
    }
    Ok(current)
}

/// [`Dir::beneath`] with `openat2`. `None` where it is unavailable: a kernel
/// without it, or a seccomp profile that refuses it.
#[cfg(target_os = "linux")]
fn openat2_beneath(dir: &Dir, rel: &Path) -> Option<io::Result<Dir>> {
    let path = match CString::new(rel.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(err) => return Some(Err(err.into())),
    };
    // SAFETY: `open_how` is plain data, for which all zeroes is the default.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = DIR_FLAGS as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
    // A rename elsewhere during the lookup makes it fail with `EAGAIN`.
    for _ in 0..4 {
        // SAFETY: `path` is NUL-terminated, `how` is a valid `open_how` of
        // the size given, and both outlive the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir.raw(),
                path.as_ptr(),
                &raw const how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            // SAFETY: `openat2` just returned this descriptor, which nothing
            // else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
            return Some(Ok(Dir { fd }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EAGAIN) => continue,
            Some(libc::ENOSYS | libc::EPERM | libc::E2BIG) => return None,
            _ => return Some(Err(err)),
        }
    }
    None
}

/// `renameat` that fails when `to` exists. `None` when this system or
/// filesystem cannot rename that way.
#[cfg(target_os = "linux")]
fn rename_exclusive(
    from_dir: RawFd,
    from: &CStr,
    to_dir: RawFd,
    to: &CStr,
) -> Option<io::Result<()>> {
    // SAFETY: both names are NUL-terminated and outlive the call.
    let renamed = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if renamed == 0 {
        return Some(Ok(()));
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EINVAL | libc::ENOSYS) => None,
        _ => Some(Err(err)),
    }
}

/// `renameat` that fails when `to` exists. `None` when this system or
/// filesystem cannot rename that way.
#[cfg(target_os = "macos")]
fn rename_exclusive(
    from_dir: RawFd,
    from: &CStr,
    to_dir: RawFd,
    to: &CStr,
) -> Option<io::Result<()>> {
    // SAFETY: both names are NUL-terminated and outlive the call.
    let renamed = unsafe {
        libc::renameatx_np(
            from_dir,
            from.as_ptr(),
            to_dir,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if renamed == 0 {
        return Some(Ok(()));
    }
    let err = io::Error::last_os_error();
    let unsupported = err
        .raw_os_error()
        .is_some_and(|code| code == libc::ENOTSUP || code == libc::EINVAL);
    if unsupported { None } else { Some(Err(err)) }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_exclusive(_: RawFd, _: &CStr, _: RawFd, _: &CStr) -> Option<io::Result<()>> {
    None
}

fn set_errno(value: libc::c_int) {
    // SAFETY: the thread's errno is always valid to write.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = value;
    }
    // SAFETY: the thread's errno is always valid to write.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    unsafe {
        *libc::__error() = value;
    }
}

fn fstat(fd: RawFd) -> io::Result<Stat> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `st` is large enough; on success `fstat` has initialized it.
    cvt(unsafe { libc::fstat(fd, st.as_mut_ptr()) })?;
    // SAFETY: initialized by the successful call above.
    Ok(Stat::from_raw(unsafe { st.assume_init_ref() }))
}

/// The open `file`'s `fstat`.
pub(crate) fn stat_file(file: &File) -> io::Result<Stat> {
    fstat(file.as_raw_fd())
}

/// Whether `err` means nothing is at a path, or what is there cannot be
/// reached without following a symlink.
pub(crate) fn absent(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) || err.raw_os_error() == Some(libc::ELOOP)
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_string())
}

/// `name` as a C string, when it is one plain name: not empty, `.` or
/// `..`, and without `/` or NUL.
fn plain_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(invalid("not a plain file name"));
    }
    CString::new(bytes).map_err(|_| invalid("not a plain file name"))
}

/// The components of the relative path `rel`, each a plain name.
fn plain_names(rel: &Path) -> io::Result<Vec<&OsStr>> {
    rel.components()
        .map(|component| match component {
            Component::Normal(name) => plain_name(name).map(|_| name),
            _ => Err(invalid("not a relative path of plain names")),
        })
        .collect()
}

/// A trusted directory (the workspace), and the paths below it, which are
/// reached without following symlinks.
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    root: PathBuf,
}

impl Tree {
    pub(crate) fn new(root: &Path) -> Tree {
        Tree {
            root: root.to_path_buf(),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The directory `path`, the root or below it.
    pub(crate) fn dir(&self, path: &Path) -> io::Result<Dir> {
        let rel = self.relative(path)?;
        Dir::open(&self.root)?.beneath(rel)
    }

    /// The directory `path` is in, and its name there. `path` is strictly
    /// below the root.
    pub(crate) fn parent(&self, path: &Path) -> io::Result<(Dir, OsString)> {
        let rel = self.relative(path)?;
        let (Some(name), Some(parent)) = (rel.file_name(), rel.parent()) else {
            return Err(invalid("not below the workspace"));
        };
        plain_name(name)?;
        let dir = Dir::open(&self.root)?.beneath(parent)?;
        Ok((dir, name.to_os_string()))
    }

    /// The entry `path`'s `lstat`.
    pub(crate) fn stat(&self, path: &Path) -> io::Result<Stat> {
        let (dir, name) = self.parent(path)?;
        dir.stat(&name)
    }

    /// `path` relative to the root, spelled with plain names only.
    fn relative<'a>(&self, path: &'a Path) -> io::Result<&'a Path> {
        let rel = path
            .strip_prefix(&self.root)
            .map_err(|_| invalid("outside the workspace"))?;
        plain_names(rel)?;
        Ok(rel)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    fn os(name: &str) -> &OsStr {
        OsStr::new(name)
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("/usr/bin/mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// `run()` on a thread that must finish within 10 seconds.
    fn bounded<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run());
        });
        rx.recv_timeout(Duration::from_secs(10)).expect("blocked")
    }

    /// `a/b/c` with a file in `c`, `outside/b/c` with one too, and `link` to
    /// `a`, `a/up` to `../outside` and `a/b/file-link` to the file.
    fn tree() -> (tempfile::TempDir, PathBuf) {
        let (d, base) = dir();
        std::fs::create_dir_all(base.join("a/b/c")).unwrap();
        std::fs::write(base.join("a/b/c/file"), "inside").unwrap();
        std::fs::create_dir_all(base.join("outside/b/c")).unwrap();
        std::fs::write(base.join("outside/b/c/file"), "outside").unwrap();
        symlink("a", base.join("link")).unwrap();
        symlink("../outside", base.join("a/up")).unwrap();
        symlink("c/file", base.join("a/b/file-link")).unwrap();
        (d, base)
    }

    #[test]
    fn a_directory_below_is_reached_by_plain_names() {
        let (_d, base) = tree();
        let root = Dir::open(&base).unwrap();
        let c = root.beneath(Path::new("a/b/c")).unwrap();
        assert_eq!(c.stat(os("file")).unwrap().kind, Kind::File);
        let again = root.beneath(Path::new("")).unwrap();
        assert!(
            again
                .stat_self()
                .unwrap()
                .same_entry(&root.stat_self().unwrap())
        );
        assert_eq!(root.stat(os("link")).unwrap().kind, Kind::Symlink);
        assert_eq!(root.stat(os("a")).unwrap().kind, Kind::Dir);
    }

    #[test]
    fn a_symlink_in_any_component_is_refused() {
        let (_d, base) = tree();
        let root = Dir::open(&base).unwrap();
        for rel in ["link", "link/b", "a/up", "a/up/b", "a/up/b/c"] {
            let err = root.beneath(Path::new(rel)).unwrap_err();
            assert!(absent(&err), "{rel}: {err}");
        }
        // The same on the component-by-component walk, which macOS uses.
        for rel in ["link/b", "a/up/b/c"] {
            let names: Vec<&OsStr> = Path::new(rel).iter().collect();
            let err = walk_beneath(&root, &names).unwrap_err();
            assert!(absent(&err), "{rel}: {err}");
        }
        let tree = Tree::new(&base);
        let err = tree.stat(&base.join("a/up/b/c/file")).unwrap_err();
        assert!(absent(&err), "{err}");
        assert!(tree.stat(&base.join("a/b/c/file")).is_ok());
        assert_eq!(
            tree.stat(&base.join("a/b/file-link")).unwrap().kind,
            Kind::Symlink
        );
    }

    #[test]
    fn only_plain_names_are_taken() {
        let (_d, base) = tree();
        let root = Dir::open(&base).unwrap();
        for rel in ["..", "a/../a", "/etc", "a/b/.."] {
            let err = root.beneath(Path::new(rel)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{rel}");
        }
        for name in ["", ".", "..", "a/b"] {
            let err = root.stat(os(name)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}");
        }
        let tree = Tree::new(&base.join("a"));
        for path in [base.join("outside/b"), base.clone(), base.join("a")] {
            let err = tree.parent(&path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{path:?}");
        }
        assert!(tree.dir(&base.join("a")).is_ok());
        let err = Dir::open(&base.join("link")).unwrap_err();
        assert!(absent(&err), "{err}");
    }

    #[test]
    fn a_directory_stays_the_one_opened_when_it_is_swapped() {
        let (_d, base) = tree();
        let tree = Tree::new(&base);
        let (b, name) = tree.parent(&base.join("a/b/c")).unwrap();
        assert_eq!(name, "c");
        // The command swaps `a` for a symlink to `outside`.
        std::fs::rename(base.join("a"), base.join("a-old")).unwrap();
        symlink("outside", base.join("a")).unwrap();
        b.remove(os("file-link"), false).unwrap();
        assert!(base.join("a-old/b/c/file").exists());
        assert!(!base.join("a-old/b/file-link").exists());
        assert!(tree.parent(&base.join("a/b/c")).is_err());
        assert_eq!(
            std::fs::read_to_string(base.join("outside/b/c/file")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn entries_lists_every_name_each_time() {
        let (_d, base) = tree();
        let a = Tree::new(&base).dir(&base.join("a")).unwrap();
        for _ in 0..2 {
            let mut names = a.entries().unwrap();
            names.sort();
            assert_eq!(names, ["b", "up"]);
        }
    }

    #[test]
    fn open_regular_refuses_anything_else_without_blocking() {
        let (_d, base) = tree();
        mkfifo(&base.join("fifo"));
        let found = bounded(move || {
            let root = Dir::open(&base).unwrap();
            let mut text = String::new();
            let (mut file, stat) = root
                .beneath(Path::new("a/b/c"))
                .unwrap()
                .open_regular(os("file"))
                .unwrap();
            file.read_to_string(&mut text).unwrap();
            let refused: Vec<bool> = ["fifo", "link", "a"]
                .iter()
                .map(|name| root.open_regular(os(name)).is_err())
                .collect();
            (text, stat.size, refused)
        });
        assert_eq!(found, ("inside".to_string(), 6, vec![true, true, true]));
    }

    #[test]
    fn nothing_is_created_through_a_symlink() {
        let (_d, base) = tree();
        let b = Tree::new(&base).dir(&base.join("a/b")).unwrap();
        let err = b.create_file(os("file-link"), 0o600).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(b.mkdir(os("file-link"), 0o700).is_err());
        assert!(b.symlink(Path::new("x"), os("file-link")).is_err());
        assert_eq!(
            std::fs::read_to_string(base.join("a/b/c/file")).unwrap(),
            "inside"
        );
        let mut file = b.create_file(os("new"), 0o640).unwrap();
        file.write_all(b"new").unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("a/b/new")).unwrap(),
            "new"
        );
        b.mkdir(os("dir"), 0o700).unwrap();
        b.symlink(Path::new("../x"), os("sym")).unwrap();
        assert_eq!(b.read_link(os("sym")).unwrap(), PathBuf::from("../x"));
        assert!(b.read_link(os("dir")).is_err());
    }

    #[test]
    fn a_rename_never_replaces_an_entry() {
        let (_d, base) = tree();
        let tree = Tree::new(&base);
        let a = tree.dir(&base.join("a")).unwrap();
        let c = tree.dir(&base.join("a/b/c")).unwrap();
        std::fs::write(base.join("a/taken"), "taken").unwrap();
        let err = c.rename_new(os("file"), &a, os("taken")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(base.join("a/taken")).unwrap(),
            "taken"
        );
        c.rename_new(os("file"), &a, os("moved")).unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("a/moved")).unwrap(),
            "inside"
        );
        let err = c.rename_new(os("file"), &a, os("again")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn without_an_exclusive_rename_a_directory_is_not_renamed() {
        // As on a filesystem that cannot rename without replacing: a
        // directory could replace an empty one, so it is refused.
        let (_d, base) = tree();
        let a = Tree::new(&base).dir(&base.join("a")).unwrap();
        let err = a.rename_looking_first(os("b"), &a, os("b2")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
        assert!(base.join("a/b").is_dir());
        std::fs::write(base.join("a/file"), "x").unwrap();
        std::fs::write(base.join("a/other"), "y").unwrap();
        let err = a
            .rename_looking_first(os("file"), &a, os("other"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        a.rename_looking_first(os("file"), &a, os("moved")).unwrap();
        assert_eq!(std::fs::read_to_string(base.join("a/moved")).unwrap(), "x");
        assert_eq!(std::fs::read_to_string(base.join("a/other")).unwrap(), "y");
    }

    #[test]
    fn directories_are_made_down_a_trusted_path_without_following_symlinks() {
        let (_d, base) = tree();
        let made = create_dirs(&base.join("new/one"), 0o700).unwrap();
        assert_eq!(made.stat_self().unwrap().kind, Kind::Dir);
        let mode = std::fs::metadata(base.join("new/one"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            create_dirs(&base.join("a/b"), 0o700).is_ok(),
            "already there"
        );
        let err = create_dirs(&base.join("link/made"), 0o700).unwrap_err();
        assert!(absent(&err), "{err}");
        assert!(!base.join("a/made").exists());
    }

    #[test]
    fn remove_and_chmod_never_follow_a_symlink() {
        let (_d, base) = tree();
        let file = base.join("a/b/c/file");
        std::fs::set_permissions(&file, PermissionsExt::from_mode(0o644)).unwrap();
        let b = Tree::new(&base).dir(&base.join("a/b")).unwrap();
        let _ = b.chmod(os("file-link"), 0o777);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        b.remove(os("file-link"), false).unwrap();
        assert!(file.exists());
        assert!(b.remove(os("c"), true).is_err(), "not empty");
        b.chmod(os("c"), 0o750).unwrap();
        let mode = std::fs::metadata(base.join("a/b/c"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o750);
        let c = b.open_dir(os("c")).unwrap();
        c.set_mode(0o700).unwrap();
        let mode = std::fs::metadata(base.join("a/b/c"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn stats_tell_entries_and_writes_apart() {
        let (_d, base) = tree();
        let c = Tree::new(&base).dir(&base.join("a/b/c")).unwrap();
        let before = c.stat(os("file")).unwrap();
        let (file, opened) = c.open_regular(os("file")).unwrap();
        assert!(before.same_entry(&opened) && before.unchanged(&opened));
        assert_eq!(stat_file(&file).unwrap(), opened);
        std::fs::write(base.join("a/b/c/file"), "written again").unwrap();
        let after = c.stat(os("file")).unwrap();
        assert!(before.same_entry(&after));
        assert!(!before.unchanged(&after));
        std::fs::remove_file(base.join("a/b/c/file")).unwrap();
        std::fs::write(base.join("a/b/c/file"), "inside").unwrap();
        let replaced = c.stat(os("file")).unwrap();
        assert!(!before.same_entry(&replaced) || before.ino == replaced.ino);
        assert_eq!(replaced.nlink, 1);
        assert_eq!(replaced.mode & !0o7777, 0);
    }
}
