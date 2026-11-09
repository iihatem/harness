//! Finding a child's binary and checking that its version is one this crate was tested with.
//!
//! Both children's protocols move with their releases (Codex's app-server is marked
//! experimental, and Claude's control messages are only documented as the SDK uses them), so a
//! version outside the tested range is refused with a message that names the range, unless the
//! caller allows untested versions.

use std::{
    ffi::OsString,
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::{io::AsyncReadExt, process::Command};

use crate::{BackendKind, DelegateError};

/// The Claude Code version the transport was tested against.
pub const CLAUDE_TESTED: &str = "2.1.312";
/// The Codex version the transport (and its schema fixtures) came from.
pub const CODEX_TESTED: &str = "0.154.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// `min <= v < max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min: Version,
    pub max: Version,
}

impl Range {
    pub fn contains(&self, v: Version) -> bool {
        self.min <= v && v < self.max
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ">={}, <{}", self.min, self.max)
    }
}

const fn v(major: u32, minor: u32, patch: u32) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

/// The versions `kind`'s transport was tested with: Claude Code 2.1.300 up to 2.2.0 (tested at
/// 2.1.312), Codex 0.154.x (tested at 0.154.0; its protocol changes between minor versions).
pub fn tested_range(kind: BackendKind) -> Range {
    match kind {
        BackendKind::Claude => Range {
            min: v(2, 1, 300),
            max: v(2, 2, 0),
        },
        BackendKind::Codex => Range {
            min: v(0, 154, 0),
            max: v(0, 155, 0),
        },
    }
}

/// The first `major.minor.patch` in `output` (`2.1.312 (Claude Code)`, `codex-cli 0.154.0`);
/// a `v` before it and a `-suffix` after it are ignored.
pub fn parse_version(output: &str) -> Option<Version> {
    output.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        let core = word.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let version = Version {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(version)
    })
}

/// The word of `output` that holds the version, as printed (with any suffix).
fn version_word(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| parse_version(w).is_some())
        .map(String::from)
}

/// Whether the printed version carries a `-suffix` or `+build` part.
fn has_suffix(output: &str) -> bool {
    version_word(output).is_some_and(|w| w.contains(['-', '+']))
}

#[derive(Debug, Clone)]
pub struct DetectOptions {
    /// The binary to use; else `PATH` is searched for the program's name.
    pub binary: Option<PathBuf>,
    /// The `PATH` to search, else the process's.
    pub path_var: Option<OsString>,
    /// Accept a version outside the tested range.
    pub allow_untested: bool,
    /// How long `--version` may take.
    pub version_timeout: Duration,
}

impl Default for DetectOptions {
    fn default() -> DetectOptions {
        DetectOptions {
            binary: None,
            path_var: None,
            allow_untested: false,
            version_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Detected {
    pub path: PathBuf,
    pub version: Version,
    /// Whether the version is inside the tested range.
    pub tested: bool,
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn find_on_path(name: &str, path_var: &OsString) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Finds `kind`'s binary, runs `--version` (stdin closed, a minimal environment: it needs no
/// credential), and checks the version.
pub async fn detect(kind: BackendKind, options: &DetectOptions) -> Result<Detected, DelegateError> {
    let path = match &options.binary {
        Some(path) => path.clone(),
        None => {
            let path_var = options
                .path_var
                .clone()
                .or_else(|| std::env::var_os("PATH"))
                .unwrap_or_default();
            find_on_path(kind.binary(), &path_var).ok_or(DelegateError::NotFound(kind.binary()))?
        }
    };
    let name = path.display().to_string();
    let output = version_output(&path, options.version_timeout)
        .await
        .unwrap_or_default();
    let version = parse_version(&output).ok_or_else(|| DelegateError::VersionUnreadable {
        binary: name,
        output: output.trim().to_string(),
    })?;
    let range = tested_range(kind);
    // A pre-release (`0.154.0-alpha.1`) is not the release that was tested.
    let tested = range.contains(version) && !has_suffix(&output);
    if !tested && !options.allow_untested {
        return Err(DelegateError::VersionOutOfRange {
            binary: kind.binary(),
            found: version_word(&output).unwrap_or_else(|| version.to_string()),
            range: range.to_string(),
        });
    }
    Ok(Detected {
        path,
        version,
        tested,
    })
}

/// What `<path> --version` printed, or `None` when it could not be run or took too long (its
/// process group is killed then).
async fn version_output(path: &Path, limit: Duration) -> Option<String> {
    let mut command = Command::new(path);
    command
        .arg("--version")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    for keep in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(keep) {
            command.env(keep, value);
        }
    }
    let mut child = command.spawn().ok()?;
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let mut stdout = child.stdout.take()?;
    let read = async {
        let mut text = String::new();
        let mut bytes = Vec::new();
        // A version line is short: cap what is read.
        let _ = (&mut stdout).take(4096).read_to_end(&mut bytes).await;
        text.push_str(&String::from_utf8_lossy(&bytes));
        let _ = child.wait().await;
        text
    };
    match tokio::time::timeout(limit, read).await {
        Ok(text) => Some(text),
        Err(_) => {
            if let Some(group) = group {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(group),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            None
        }
    }
}
