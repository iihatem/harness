use std::path::PathBuf;

pub use harness_core::permission::FsAccess;

/// The sandbox for one command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Filesystem write access mode.
    pub access: FsAccess,
    /// The workspace directory. Reads are allowed everywhere; writes under it only with
    /// [`FsAccess::WorkspaceWrite`]. Need not be canonical: the backends canonicalize it.
    pub workspace: PathBuf,
    /// Extra directories writable with [`FsAccess::WorkspaceWrite`], beyond the workspace and the
    /// standard temp directories. Paths that do not exist are skipped.
    pub extra_writable: Vec<PathBuf>,
    /// Allow loopback (localhost) networking. This does not open up AF_UNIX sockets: on macOS the
    /// only one a command can connect to, in either mode, is the syslog socket. Honoured on macOS
    /// only: Linux seccomp cannot tell loopback from other addresses, so the Linux backend always
    /// denies non-AF_UNIX sockets.
    pub allow_localhost: bool,
}
