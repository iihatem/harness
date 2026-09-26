use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::policy::{FsAccess, SandboxPolicy};

/// Base profile: reads everywhere, no network, no writes except the usual
/// device files. Shared by both [`FsAccess`] modes. Adapted from
/// `scratchpad/sbx/final-base.sb`.
const BASE_PROFILE: &str = r#"(version 1)
(deny default (with message (param "LOG_TAG")))

; processes: children inherit this sandbox
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

; reads: everywhere
(allow file-read*)

; device files
(allow file-write-data file-ioctl
  (require-all (path "/dev/null") (vnode-type CHARACTER-DEVICE)))
(allow file-write-data (literal "/dev/zero") (literal "/dev/stdout") (literal "/dev/stderr")
  (regex #"^/dev/fd/[0-9]+$"))
(allow file-write-data file-ioctl (literal "/dev/tty") (literal "/dev/dtracehelper"))

; pty (openpty, interactive tools)
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-read* file-write* (require-all (regex #"^/dev/ttys[0-9]+$") (extension "com.apple.sandbox.pty")))
(allow file-ioctl (regex #"^/dev/ttys[0-9]+$"))

; sysctl: reads are informational; two "writes" are really reads (Java, V8)
(allow sysctl-read)
(allow sysctl-write (sysctl-name "kern.grade_cputype") (sysctl-name "kern.tcsm_enable"))

; IPC used by Python multiprocessing / OpenMP
(allow ipc-posix-sem)
(allow ipc-posix-shm-read-data ipc-posix-shm-write-create ipc-posix-shm-write-unlink
  (ipc-posix-name-regex #"^/__KMP_REGISTERED_LIB_[0-9]+$"))
(allow ipc-posix-shm-read* (ipc-posix-name-prefix "apple.cfprefs.") (ipc-posix-name "apple.shm.notification_center"))
(allow iokit-open (iokit-registry-entry-class "RootDomainUserClient"))

; preferences (read-only) + minimal system services
(allow user-preference-read)
(allow mach-lookup
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (local-name "com.apple.cfprefsd.agent")
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.opendirectoryd.membership")
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.system.logger")
  (global-name "com.apple.logd")
  (global-name "com.apple.system.notification_center")
  (global-name "com.apple.PowerManagement.control"))
(allow network-outbound (literal "/private/var/run/syslog"))
"#;

/// Loopback-only network block, appended when `allow_localhost` is set.
/// Adapted from the equivalent block in codex-rs's seatbelt profile
/// builder (`dynamic_network_policy_for_network`).
const LOCALHOST_BLOCK: &str = r#"
; ---- allow_localhost: AF_UNIX + loopback only, no other network ----
(allow system-socket (socket-domain AF_UNIX))
(allow network-bind (local ip "*:*"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
"#;

/// Canonicalized paths needed to fill in the profile's `-D` params.
struct CanonPaths {
    workspace: PathBuf,
    tmpdir: PathBuf,
    user_cache_dir: PathBuf,
    extra_writable: Vec<PathBuf>,
}

impl CanonPaths {
    fn resolve(policy: &SandboxPolicy) -> io::Result<Self> {
        let workspace = std::fs::canonicalize(&policy.workspace).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("canonicalizing workspace {:?}: {e}", policy.workspace),
            )
        })?;

        let tmpdir_raw = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let tmpdir = std::fs::canonicalize(&tmpdir_raw).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("canonicalizing TMPDIR {tmpdir_raw:?}: {e}"),
            )
        })?;

        let user_cache_dir = darwin_user_cache_dir()?;

        // Non-existent extra_writable paths are silently skipped.
        let extra_writable = policy
            .extra_writable
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .collect();

        Ok(Self {
            workspace,
            tmpdir,
            user_cache_dir,
            extra_writable,
        })
    }
}

/// Runs `getconf DARWIN_USER_CACHE_DIR` once per process and caches the
/// canonicalized result.
fn darwin_user_cache_dir() -> io::Result<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();

    let cached = CACHE.get_or_init(|| {
        let output = std::process::Command::new("getconf")
            .arg("DARWIN_USER_CACHE_DIR")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let raw = String::from_utf8(output.stdout).ok()?;
        std::fs::canonicalize(raw.trim()).ok()
    });

    cached
        .clone()
        .ok_or_else(|| io::Error::other("failed to determine DARWIN_USER_CACHE_DIR via getconf"))
}

/// Generates a per-invocation tag used as the Seatbelt `(with message ...)`
/// value on every deny rule, so unified-log denial entries can be
/// correlated back to the command that produced them.
fn generate_log_tag(program: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let basename = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("cmd");
    let sanitized: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("harness.{}.{}.{}", sanitized, std::process::id(), n)
}

/// Renders a path as a profile param value.
fn path_param(path: &Path) -> io::Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not valid UTF-8: {path:?}"),
        )
    })
}

/// The write-access section appended for [`FsAccess::WorkspaceWrite`]:
/// writable roots plus protections for workspace metadata that must stay
/// intact even though the workspace is otherwise writable. Adapted from
/// `scratchpad/sbx/final-write.sb`.
fn write_section(extra_keys: &[String]) -> String {
    let mut out = String::new();
    out.push_str("\n; ---- workspace-write: writable roots ----\n(allow file-write*\n");
    out.push_str("  (subpath (param \"WORKSPACE\"))\n");
    out.push_str("  (subpath (param \"TMPDIR\"))\n");
    out.push_str("  (subpath (param \"USER_CACHE_DIR\"))\n");
    out.push_str("  (subpath \"/private/tmp\")\n");
    out.push_str("  (subpath \"/private/var/tmp\")\n");
    for key in extra_keys {
        out.push_str(&format!("  (subpath (param \"{key}\"))\n"));
    }
    out.push_str(")\n\n");
    out.push_str("; protected metadata inside the workspace (later rules win)\n");
    out.push_str("(deny file-write* (with message (param \"LOG_TAG\"))\n");
    out.push_str("  (regex (string-append \"^\" (regex-quote (param \"WORKSPACE\"))\n");
    out.push_str(
        "                        \"(/.*)?/\\\\.git(/modules/.+)?/(config|hooks(/.*)?)$\"))\n",
    );
    out.push_str("  (literal (string-append (param \"WORKSPACE\") \"/.git\"))\n");
    out.push_str("  (subpath (string-append (param \"WORKSPACE\") \"/.harness\"))\n");
    out.push_str("  (literal (string-append (param \"WORKSPACE\") \"/HEAD\")))\n");
    out.push_str("(deny file-write-unlink (with message (param \"LOG_TAG\"))\n");
    out.push_str("  (literal (param \"WORKSPACE\")))\n");
    out
}

/// Builds the full Seatbelt profile text and the `-D key=value` params it
/// references, for the given policy and program (the program name only
/// feeds the log tag).
pub(crate) fn build_profile(
    policy: &SandboxPolicy,
    program: &str,
) -> io::Result<(String, Vec<(String, String)>)> {
    let mut profile = String::from(BASE_PROFILE);
    let mut params = vec![("LOG_TAG".to_string(), generate_log_tag(program))];

    if policy.access == FsAccess::WorkspaceWrite {
        let canon = CanonPaths::resolve(policy)?;

        let mut extra_keys = Vec::with_capacity(canon.extra_writable.len());
        for (i, path) in canon.extra_writable.iter().enumerate() {
            let key = format!("EXTRA_{i}");
            params.push((key.clone(), path_param(path)?));
            extra_keys.push(key);
        }

        params.push(("WORKSPACE".to_string(), path_param(&canon.workspace)?));
        params.push(("TMPDIR".to_string(), path_param(&canon.tmpdir)?));
        params.push((
            "USER_CACHE_DIR".to_string(),
            path_param(&canon.user_cache_dir)?,
        ));

        profile.push_str(&write_section(&extra_keys));
    }

    if policy.allow_localhost {
        profile.push_str(LOCALHOST_BLOCK);
    }

    Ok((profile, params))
}
