use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::policy::{FsAccess, SandboxPolicy};
use crate::roots::{home_dir, safe_root};

/// Absolute path, so a `getconf` earlier on PATH can never pick a writable root.
const GETCONF_PATH: &str = "/usr/bin/getconf";

/// Base profile: reads everywhere, no network, no writes except the usual
/// device files. Shared by both [`FsAccess`] modes. Setuid binaries such as
/// `/bin/ps` and `/usr/bin/crontab` fail to exec under it (EPERM).
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
/// builder (`dynamic_network_policy_for_network`). It does not widen AF_UNIX
/// access: in either mode the only AF_UNIX socket a command can connect to is
/// syslog's (`/private/var/run/syslog`).
const LOCALHOST_BLOCK: &str = r#"
; ---- allow_localhost: loopback IP only, no other network ----
(allow system-socket (socket-domain AF_UNIX))
(allow network-bind (local ip "*:*"))
(allow network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
"#;

/// Canonicalized paths needed to fill in the profile's `-D` params. The
/// temp and cache roots are optional: when none can be found safely, that
/// root is left out (`/private/tmp` and `/private/var/tmp` stay writable).
struct CanonPaths {
    workspace: PathBuf,
    tmpdir: Option<PathBuf>,
    user_cache_dir: Option<PathBuf>,
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

        let home = home_dir();
        let home = home.as_deref();
        let tmpdir = tmpdir_root(std::env::var_os("TMPDIR").as_deref(), home, || {
            user_temp_dir(home)
        });
        let user_cache_dir = user_cache_dir(home);

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

/// The `TMPDIR` root: `env_tmpdir` if [`safe_root`] accepts it, otherwise
/// `fallback()` (the per-user temp dir from `getconf`), otherwise none.
fn tmpdir_root(
    env_tmpdir: Option<&OsStr>,
    home: Option<&Path>,
    fallback: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    env_tmpdir
        .and_then(|tmpdir| safe_root(Path::new(tmpdir), home))
        .or_else(fallback)
}

/// Returns the value cached in `cell`, or computes it. Only successes are
/// cached, so a failed lookup is retried on the next call.
fn cached(cell: &OnceLock<PathBuf>, compute: impl FnOnce() -> Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = cell.get() {
        return Some(path.clone());
    }
    let path = compute()?;
    Some(cell.get_or_init(|| path).clone())
}

/// `/usr/bin/getconf <name>` for one of the per-user `DARWIN_USER_*_DIR`
/// values, validated by [`safe_root`].
fn getconf_dir(name: &str, home: Option<&Path>) -> Option<PathBuf> {
    let output = std::process::Command::new(GETCONF_PATH)
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    safe_root(Path::new(raw), home)
}

/// The per-user cache dir (`DARWIN_USER_CACHE_DIR`), cached once found.
fn user_cache_dir(home: Option<&Path>) -> Option<PathBuf> {
    static CACHE: OnceLock<PathBuf> = OnceLock::new();
    cached(&CACHE, || getconf_dir("DARWIN_USER_CACHE_DIR", home))
}

/// The per-user temp dir (`DARWIN_USER_TEMP_DIR`), cached once found.
fn user_temp_dir(home: Option<&Path>) -> Option<PathBuf> {
    static CACHE: OnceLock<PathBuf> = OnceLock::new();
    cached(&CACHE, || getconf_dir("DARWIN_USER_TEMP_DIR", home))
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
/// writable roots, a guard against writing through hard links, and
/// protections for workspace metadata that must stay intact even though the
/// workspace is otherwise writable. Later rules win, so the order matters.
///
/// `root_keys` are the params of the optional writable roots (TMPDIR,
/// USER_CACHE_DIR, EXTRA_n) that were found; the workspace, `/private/tmp`
/// and `/private/var/tmp` are always writable.
///
/// Name matching: on a case-insensitive APFS volume, Seatbelt applied these
/// `literal`/`regex` rules case-insensitively and with the volume's Unicode
/// folding (`.GIT`, `head`, `.harneſs` and `hooKs` with a Kelvin sign were all
/// denied, including names that did not exist yet), so the rules are written
/// in lowercase. On a case-sensitive volume those variants are different
/// names, which git ignores.
fn write_section(root_keys: &[String]) -> String {
    let mut roots = String::from(r#"(subpath (param "WORKSPACE"))"#);
    for key in root_keys {
        roots.push_str(&format!("\n    (subpath (param \"{key}\"))"));
    }
    roots.push_str("\n    (subpath \"/private/tmp\")\n    (subpath \"/private/var/tmp\")");

    format!(
        r#"
; ---- workspace-write: writable roots ----
(allow file-write*
    {roots})

; ---- hard links ----
; Seatbelt checks paths, not inodes: a hard link inside a writable root whose
; other name is outside would let writes reach that outside file. link()
; needs write access to its source, so no new link to an outside or protected
; file can be made here; these rules cover links that already exist. Deny
; every write to a regular file with more than one name. This must be all of
; file-write*: utimes is not covered by file-write-times alone. Removing the
; extra name cannot change the other one, so unlink stays allowed in the
; roots. A side effect is that renaming such a file is denied.
(deny file-write* (with message (param "LOG_TAG"))
  (require-all (vnode-type REGULAR-FILE) (file-attribute has-multiple-names)))
(allow file-write-unlink
  (require-all (vnode-type REGULAR-FILE) (file-attribute has-multiple-names)
    (require-any
    {roots})))

; ---- protected metadata inside the workspace ----
; Every `.git` entry at any depth (dir, gitfile or symlink: no create,
; rename, replace or delete), the whole `.harness/` dir, and a top-level
; `HEAD` (which would make the workspace look like a bare repo).
;
; In every gitdir -- any `.git`, `.git/modules/*` (submodules) and
; `.git/worktrees/<id>` (linked worktrees) -- the files that decide where git
; loads config and hooks from, or that hold them:
; - `config` and `hooks/`;
; - `commondir`: git reads it in any gitdir and then takes config and hooks
;   from the dir it names (setup.c get_common_dir_noenv, path.c common_list);
; - `config.worktree`: read once extensions.worktreeConfig is set;
; - in `.git/worktrees/<id>`, config and hooks are ignored while `commondir`
;   is there, and git falls back to them if it is not, so they are protected
;   too.
; Git writes none of these during commit, checkout, switch or stash; other
; writes inside `.git` stay allowed so those keep working. Denied as a result:
; `git worktree add` (creates `commondir`), `git worktree remove/prune`
; (delete it), and `git config --worktree` or `git sparse-checkout` once
; worktreeConfig is set (write `config.worktree`).
; These are path rules: moving a parent dir (a nested repo, `.git/modules/*`,
; `.git/worktrees/<id>`) out to a writable root, editing it there and moving
; it back is not covered. The top-level `.git` cannot be moved.
(deny file-write* (with message (param "LOG_TAG"))
  (regex (string-append "^" (regex-quote (param "WORKSPACE")) "(/.*)?/\\.git$"))
  (regex (string-append "^" (regex-quote (param "WORKSPACE"))
                        "(/.*)?/\\.git(/modules/.+|/worktrees/[^/]+)?"
                        "/(config|config\\.worktree|commondir|hooks(/.*)?)$"))
  (subpath (string-append (param "WORKSPACE") "/.harness"))
  (literal (string-append (param "WORKSPACE") "/HEAD")))
(deny file-write-unlink (with message (param "LOG_TAG"))
  (literal (param "WORKSPACE")))
"#
    )
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
        params.push(("WORKSPACE".to_string(), path_param(&canon.workspace)?));

        let optional = [
            ("TMPDIR".to_string(), canon.tmpdir),
            ("USER_CACHE_DIR".to_string(), canon.user_cache_dir),
        ]
        .into_iter()
        .filter_map(|(key, path)| Some((key, path?)));
        let extra = canon
            .extra_writable
            .into_iter()
            .enumerate()
            .map(|(i, path)| (format!("EXTRA_{i}"), path));

        let mut root_keys = Vec::new();
        for (key, path) in optional.chain(extra) {
            params.push((key.clone(), path_param(&path)?));
            root_keys.push(key);
        }

        profile.push_str(&write_section(&root_keys));
    }

    if policy.allow_localhost {
        profile.push_str(LOCALHOST_BLOCK);
    }

    Ok((profile, params))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn canon_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        (dir, canon)
    }

    #[test]
    fn tmpdir_uses_a_valid_env_value() {
        let (_d, home) = canon_tempdir();
        let (_e, tmp) = canon_tempdir();
        let got = tmpdir_root(Some(tmp.as_os_str()), Some(&home), || {
            panic!("fallback must not run")
        });
        assert_eq!(got, Some(tmp));
    }

    #[test]
    fn tmpdir_falls_back_when_env_is_unset_or_too_broad() {
        let (_d, home) = canon_tempdir();
        let (_e, fallback) = canon_tempdir();
        let parent = home.parent().unwrap().to_path_buf();
        for env in [
            None,
            Some(Path::new("/").as_os_str()),
            Some(home.as_os_str()),
            Some(parent.as_os_str()),
            Some(std::ffi::OsStr::new(".")),
            Some(std::ffi::OsStr::new("/nonexistent-harness-tmpdir")),
        ] {
            let got = tmpdir_root(env, Some(&home), || Some(fallback.clone()));
            assert_eq!(got, Some(fallback.clone()), "TMPDIR={env:?}");
        }
    }

    #[test]
    fn tmpdir_is_none_when_env_and_fallback_both_fail() {
        let (_d, home) = canon_tempdir();
        let got = tmpdir_root(Some(Path::new("/").as_os_str()), Some(&home), || None);
        assert_eq!(got, None);
    }

    #[test]
    fn cached_only_keeps_successes() {
        let cell = OnceLock::new();
        let calls = Cell::new(0);
        let compute = |v: Option<PathBuf>| {
            let calls = &calls;
            move || {
                calls.set(calls.get() + 1);
                v
            }
        };
        assert_eq!(cached(&cell, compute(None)), None);
        assert_eq!(
            cached(&cell, compute(Some(PathBuf::from("/a")))),
            Some(PathBuf::from("/a"))
        );
        assert_eq!(
            cached(&cell, compute(Some(PathBuf::from("/b")))),
            Some(PathBuf::from("/a"))
        );
        assert_eq!(calls.get(), 2, "a failure is retried; a success is reused");
    }

    #[test]
    fn darwin_user_dirs_resolve_on_this_host() {
        let home = home_dir();
        for dir in [
            user_cache_dir(home.as_deref()),
            user_temp_dir(home.as_deref()),
        ] {
            let dir = dir.expect("getconf should report a per-user dir on macOS");
            assert!(dir.is_absolute() && dir.is_dir(), "{dir:?}");
        }
    }

    #[test]
    fn optional_roots_are_left_out_of_the_profile() {
        let section = write_section(&[]);
        assert!(!section.contains("TMPDIR"), "{section}");
        assert!(!section.contains("USER_CACHE_DIR"), "{section}");
        let section = write_section(&["TMPDIR".into(), "EXTRA_0".into()]);
        assert!(
            section.contains("(subpath (param \"TMPDIR\"))"),
            "{section}"
        );
        assert!(
            section.contains("(subpath (param \"EXTRA_0\"))"),
            "{section}"
        );
    }
}
