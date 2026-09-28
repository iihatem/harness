//! Chooses the git-protection tier: a throwaway child tries the full tier's
//! exact setup (`mountns.rs`) on a temporary directory.

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use harness_core::tool::GitProtection;

use super::{mountns, mountplan};

/// The git-protection tier this host supports, probed once per process: the
/// full tier where a child can set up a user and mount namespace with a
/// read-only bind in it, the basic tier (saying why) where it cannot.
pub fn linux_git_protection() -> GitProtection {
    static TIER: OnceLock<GitProtection> = OnceLock::new();
    TIER.get_or_init(|| match probe() {
        Ok(()) => GitProtection::Full,
        Err(reason) => GitProtection::Basic { reason },
    })
    .clone()
}

/// Runs `/bin/sh` in a child that first sets up a user and mount namespace
/// with a pinned directory and a read-only file in it, then tries to write
/// the file. `Err` says why the full tier is unavailable.
///
/// The child stays in harness's session and is waited for here, so the
/// basic tier's reaper (`crate::procs`) never takes it.
fn probe() -> Result<(), String> {
    let dir =
        ProbeDir::create().map_err(|e| format!("the probe could not create a directory: {e}"))?;
    let plan = mountplan::probe(dir.path())
        .ok_or_else(|| "the probe could not describe its directory".to_string())?;
    let (reader, writer) =
        mountplan::setup_pipe().map_err(|e| format!("the probe could not make a pipe: {e}"))?;
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", ": > pin/file 2>/dev/null && exit 10; exit 0"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `mountns::enter` is async-signal-safe (see its module docs);
    // the closure owns `plan` and the pipe's write end.
    unsafe {
        cmd.pre_exec(move || mountns::enter(&plan, Some(writer.as_raw_fd())));
    }
    match cmd.status() {
        Ok(status) if status.code() == Some(0) => Ok(()),
        Ok(status) if status.code() == Some(10) => {
            Err("a read-only bind mount did not stop a write".into())
        }
        Ok(status) => Err(format!("the probe failed ({status})")),
        Err(e) => Err(match mountplan::read_failure(&reader) {
            Some(failure) => {
                failure.describe(&[dir.path().join("pin"), dir.path().join("pin/file")])
            }
            None => format!("the probe could not start: {e}"),
        }),
    }
}

/// `<temp>/harness-userns-probe-<pid>-<n>/pin/file`, removed on drop.
struct ProbeDir(PathBuf);

impl ProbeDir {
    fn create() -> std::io::Result<ProbeDir> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // Removed on drop from here on, whatever fails below.
        let mut dir = ProbeDir(std::env::temp_dir().join(format!(
            "harness-userns-probe-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir_all(dir.0.join("pin"))?;
        dir.0 = dir.0.canonicalize()?;
        std::fs::write(dir.0.join("pin/file"), b"probe")?;
        Ok(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
