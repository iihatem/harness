//! `harness sandbox doctor`: which sandbox this system gets, how git metadata is protected, and
//! how to get the Linux full tier. It only reads; it never changes system files.

use std::path::{Path, PathBuf};

use harness_config::config::LinuxGitProtection;
use harness_core::tool::GitProtection;

use crate::{setup, term::terminal_safe};

pub fn run() -> u8 {
    let (setting, quarantine) = match setup::load() {
        Ok(setup) => (
            setup.config.linux_git_protection,
            Some(setup.paths.data_dir.join("quarantine")),
        ),
        Err(message) => {
            eprintln!("warning: {}", terminal_safe(&message));
            (LinuxGitProtection::default(), None)
        }
    };
    let detected = harness_sandbox::detect(harness_sandbox::SandboxSettings::default());
    let facts = Facts {
        linux: cfg!(target_os = "linux"),
        mechanism: detected.as_ref().map(|s| s.name()),
        unavailable: detected.is_none().then(harness_sandbox::unavailable_reason),
        protection: detected.as_ref().map(|s| s.git_protection()),
        disabled: std::env::var("HARNESS_SANDBOX").as_deref() == Ok("none"),
        setting,
        quarantine,
        host: Host::read(),
        subreaper: subreaper_status(),
        watcher: watcher_status(),
        pidfds: pidfd_status(),
        exe: std::env::current_exe()
            .ok()
            .and_then(|exe| exe.canonicalize().ok()),
    };
    print!("{}", render(&facts));
    0
}

/// Whether harness has managed to become a child subreaper ([`harness_sandbox::subreaper_active`]),
/// for the doctor's report. `harness_sandbox::detect` always attempts `prctl` as soon as it builds
/// a Linux sandbox (in both tiers, since Task 8's R2), and `run` calls `detect` before asking this,
/// so by the time it is asked here the kernel has already been asked for real: `NotAsked` is only
/// the type's default (used when there is no Linux sandbox to ask on behalf of, in which case this
/// is never rendered at all) and in tests. Never constructed on a non-Linux host: `#[allow
/// (dead_code)]` on the enum accepts that as intentional, not a bug to fix.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Subreaper {
    /// No Linux sandbox has asked `prctl` on this process's behalf.
    #[default]
    NotAsked,
    Active,
    Refused,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn subreaper_status() -> Subreaper {
    if harness_sandbox::subreaper_active() {
        Subreaper::Active
    } else {
        Subreaper::Refused
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn subreaper_status() -> Subreaper {
    Subreaper::NotAsked
}

/// A watcher start failure seen so far in this process ([`harness_sandbox::watcher_failures`]),
/// for the doctor's report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatcherStatus {
    pub failures: u64,
    pub last_error: Option<String>,
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn watcher_status() -> WatcherStatus {
    let (failures, last_error) = harness_sandbox::watcher_failures();
    WatcherStatus {
        failures,
        last_error,
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn watcher_status() -> WatcherStatus {
    WatcherStatus::default()
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn pidfd_status() -> bool {
    harness_sandbox::reaps_through_pidfds()
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn pidfd_status() -> bool {
    true
}

/// What the report is made from.
#[derive(Debug, Clone)]
pub struct Facts {
    pub linux: bool,
    pub mechanism: Option<&'static str>,
    pub unavailable: Option<String>,
    pub protection: Option<GitProtection>,
    pub disabled: bool,
    pub setting: LinuxGitProtection,
    pub quarantine: Option<PathBuf>,
    pub host: Host,
    pub subreaper: Subreaper,
    pub watcher: WatcherStatus,
    /// Whether orphans are, so far, reaped through pidfds rather than the by-pid fallback.
    /// Defaults to `true`: nothing has been refused until it is.
    pub pidfds: bool,
    pub exe: Option<PathBuf>,
}

impl Default for Facts {
    fn default() -> Facts {
        Facts {
            linux: false,
            mechanism: None,
            unavailable: None,
            protection: None,
            disabled: false,
            setting: LinuxGitProtection::default(),
            quarantine: None,
            host: Host::default(),
            subreaper: Subreaper::default(),
            watcher: WatcherStatus::default(),
            pidfds: true,
            exe: None,
        }
    }
}

/// Kernel settings and container markers that decide whether user namespaces work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Host {
    /// `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu).
    pub apparmor_restrict: Option<String>,
    /// `kernel.unprivileged_userns_clone` (Debian's older kernels).
    pub userns_clone: Option<String>,
    /// `user.max_user_namespaces`.
    pub max_user_namespaces: Option<String>,
    /// A Docker or Podman container marker exists.
    pub container: bool,
}

impl Host {
    fn read() -> Host {
        let sysctl = |path: &str| {
            std::fs::read_to_string(path)
                .ok()
                .map(|value| value.trim().to_string())
        };
        Host {
            apparmor_restrict: sysctl("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
            userns_clone: sysctl("/proc/sys/kernel/unprivileged_userns_clone"),
            max_user_namespaces: sysctl("/proc/sys/user/max_user_namespaces"),
            container: Path::new("/.dockerenv").exists()
                || Path::new("/run/.containerenv").exists(),
        }
    }
}

/// The report. Every value that comes from the system is escaped for the terminal.
pub fn render(facts: &Facts) -> String {
    let mut out = String::new();
    match facts.mechanism {
        Some(name) => out.push_str(&format!("Sandbox: {name}\n")),
        None => out.push_str(&format!(
            "Sandbox: none ({}); every shell command will need approval\n",
            terminal_safe(facts.unavailable.as_deref().unwrap_or("unknown reason"))
        )),
    }
    if facts.disabled {
        out.push_str(
            "HARNESS_SANDBOX=none is set, so harness runs without the sandbox and asks before every shell command.\n",
        );
    }
    let Some(protection) = &facts.protection else {
        return out;
    };
    let reason = match protection {
        GitProtection::Full if facts.linux => {
            out.push_str(
                "Git metadata protection: full tier (read-only mounts in a user namespace)\n",
            );
            // Since the full tier also snapshots and restores every protected file (a rename or
            // unlink from outside a command's mounts detaches its bind), the same subreaper,
            // watcher and pidfd machinery that the basic tier depends on is at work here too.
            out.push_str(&subreaper_note(facts.subreaper));
            out.push_str(&watcher_note(&facts.watcher));
            out.push_str(&pidfd_note(facts.pidfds));
            out.push_str(&background_processes_note());
            return out;
        }
        GitProtection::Full => {
            out.push_str("Git metadata protection: full (writes to git hooks and config fail inside the sandbox)\n");
            return out;
        }
        GitProtection::Basic { reason } => reason,
    };
    out.push_str("Git metadata protection: basic tier\n");
    out.push_str(&format!("  Why: {}\n", terminal_safe(reason)));
    out.push_str(
        "  In this tier harness checks git metadata after each command: it moves new hooks, config and\n  repositories to quarantine and restores changed files. A git process running outside the\n  sandbox, such as an editor's, could still read a planted file in the moment before that.\n",
    );
    out.push_str(&subreaper_note(facts.subreaper));
    if let Some(quarantine) = &facts.quarantine {
        out.push_str(&format!(
            "  Quarantined files go to {}\n",
            terminal_safe(&quarantine.display().to_string())
        ));
    }
    out.push_str(match facts.setting {
        LinuxGitProtection::BestEffort => {
            "  sandbox.linux_git_protection = \"best-effort\": commands run in this tier. Set it to \"required\"\n  to have every shell command ask first instead.\n"
        }
        LinuxGitProtection::Required => {
            "  sandbox.linux_git_protection = \"required\": every shell command asks first in this tier.\n"
        }
    });
    out.push_str(&watcher_note(&facts.watcher));
    out.push_str(&pidfd_note(facts.pidfds));
    out.push_str(&background_processes_note());
    out.push_str("\nTo get the full tier, harness must be able to create user namespaces.\n");
    let mut fixes = 0;
    if facts.host.userns_clone.as_deref() == Some("0") {
        fixes += 1;
        out.push_str(&sysctl_fix(
            "They are turned off (kernel.unprivileged_userns_clone = 0). To turn them on:",
            "kernel.unprivileged_userns_clone",
            "1",
        ));
    }
    if facts.host.max_user_namespaces.as_deref() == Some("0") {
        fixes += 1;
        out.push_str(&sysctl_fix(
            "They are limited to none (user.max_user_namespaces = 0). To allow them:",
            "user.max_user_namespaces",
            "10000",
        ));
    }
    if facts.host.apparmor_restrict.as_deref() == Some("1") {
        fixes += 1;
        out.push_str(&apparmor_fix(facts.exe.as_deref()));
    }
    if reason.contains("securebit") || reason.contains("SECBIT") {
        fixes += 1;
        out.push_str(
            "\nA locked securebit is refusing part of the sandbox's setup (for example systemd's\nservice-level `noroot-locked` without `noroot`). Remove that lock, or run harness from a unit\nor shell that does not set it.\n",
        );
    }
    if facts.host.container {
        fixes += 1;
        out.push_str(
            "\nharness is running in a container, and container runtimes block user namespaces by default\n(Docker's seccomp profile does). The full tier needs the container started with a profile that\nallows them, for example `docker run --security-opt seccomp=unconfined --security-opt\napparmor=unconfined ...`, which lifts those limits for everything in the container.\n",
        );
    }
    if fixes == 0 {
        out.push_str(
            "\nharness could not tell what blocks them here. Check `dmesg` for AppArmor or SELinux denials\naround the step named above, and the sysctls kernel.unprivileged_userns_clone and\nuser.max_user_namespaces.\n",
        );
    }
    out.push_str("\nharness does not change any of these settings itself.\n");
    out
}

/// The subreaper carry-forward note (task-6/7/8): what a fresh `doctor` process can and cannot
/// know about `prctl(PR_SET_CHILD_SUBREAPER)`.
fn subreaper_note(subreaper: Subreaper) -> String {
    match subreaper {
        Subreaper::Active => String::from(
            "  harness has registered as a subreaper here, so processes a command leaves running stay its\n  descendants and are tracked between commands.\n",
        ),
        Subreaper::Refused => String::from(
            "  The kernel refused to make harness a subreaper (prctl(PR_SET_CHILD_SUBREAPER) failed), so\n  processes a command leaves running are always assumed present: protected files are restored\n  before every command rather than only when something is still running.\n",
        ),
        Subreaper::NotAsked => String::from(
            "  Whether harness can become a subreaper (measured during a session, once a command has run):\n  if the kernel refuses, processes a command leaves running are always assumed present, so\n  protected files are restored before every command.\n",
        ),
    }
}

fn watcher_note(watcher: &WatcherStatus) -> String {
    if watcher.failures == 0 {
        return String::from(
            "  The watcher that checks git metadata as it changes while a command runs (measured during a session): not yet started, or starting cleanly so far.\n",
        );
    }
    let last = watcher
        .last_error
        .as_deref()
        .map(terminal_safe)
        .unwrap_or_else(|| "unknown error".to_string());
    format!(
        "  The watcher that checks git metadata as it changes could not start {} time(s) here; the last\n  error was: {last}. This can mean an inotify instance limit; raise it with:\n\n    sudo sysctl -w fs.inotify.max_user_instances=1024\n",
        watcher.failures
    )
}

/// Whether orphans an earlier command left running are reaped through pidfds, or through the
/// `waitpid`-by-pid fallback (`harness_sandbox::reaps_through_pidfds`, Task 7's M9). The fallback
/// is only ever taken when the kernel refuses `pidfd_open` (`ENOSYS`/`EPERM`); it is not itself a
/// problem, so this is informational, not a fix.
fn pidfd_note(pidfds: bool) -> String {
    if pidfds {
        String::new()
    } else {
        String::from(
            "  This kernel refused pidfd_open, so harness reaps processes left running by their pid\n  instead of a pidfd; harmless, but it means a reused pid could in principle be reaped as if it\n  were the process harness meant.\n",
        )
    }
}

fn background_processes_note() -> String {
    String::from(
        "  Daemons a command starts (gpg-agent, git credential-cache, git's detached auto-gc) keep\n  \"processes left running\" alive, so restoring while they run stays on until they exit.\n",
    )
}

fn sysctl_fix(what: &str, key: &str, value: &str) -> String {
    format!(
        "\n{what}\n\n    sudo sysctl -w {key}={value}\n    echo '{key} = {value}' | sudo tee /etc/sysctl.d/60-harness-userns.conf\n"
    )
}

/// Ubuntu's two ways out: an AppArmor profile that lets only this binary create user namespaces,
/// or the sysctl for every program.
fn apparmor_fix(exe: Option<&Path>) -> String {
    let mut out = String::from(
        "\nUbuntu restricts them with AppArmor (kernel.apparmor_restrict_unprivileged_userns = 1). Either:\n\n1. Allow them for this harness binary only (recommended): save this profile and load it.\n\n",
    );
    let path = exe
        .and_then(Path::to_str)
        .filter(|p| !p.contains(['"', '\\']) && !p.chars().any(char::is_control));
    match path {
        Some(path) => out.push_str(&format!(
            "    sudo tee /etc/apparmor.d/harness > /dev/null <<'EOF'\n    abi <abi/4.0>,\n    include <tunables/global>\n\n    profile harness \"{}\" flags=(unconfined) {{\n      userns,\n\n      include if exists <local/harness>\n    }}\n    EOF\n    sudo apparmor_parser -r /etc/apparmor.d/harness\n\n   Do it again if harness moves to another path.\n",
            terminal_safe(path)
        )),
        None => out.push_str(
            "   harness's path cannot be written into an AppArmor profile as it is. Install harness at a\n   plain path such as /usr/local/bin/harness and run this command again.\n",
        ),
    }
    out.push_str(&sysctl_fix(
        "2. Or allow them for every program:",
        "kernel.apparmor_restrict_unprivileged_userns",
        "0",
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(host: Host) -> Facts {
        Facts {
            linux: true,
            mechanism: Some("landlock+seccomp"),
            protection: Some(GitProtection::Basic {
                reason: "writing /proc/self/setgroups failed: Operation not permitted (os error 1)"
                    .into(),
            }),
            quarantine: Some(PathBuf::from("/home/u/.local/share/harness/quarantine")),
            host,
            exe: Some(PathBuf::from("/home/u/.cargo/bin/harness")),
            ..Facts::default()
        }
    }

    #[test]
    fn ubuntu_gets_the_apparmor_profile_and_the_sysctl() {
        let out = render(&basic(Host {
            apparmor_restrict: Some("1".into()),
            max_user_namespaces: Some("63000".into()),
            ..Host::default()
        }));
        assert!(
            out.starts_with("Sandbox: landlock+seccomp\nGit metadata protection: basic tier\n"),
            "{out}"
        );
        assert!(out.contains(
            "  Why: writing /proc/self/setgroups failed: Operation not permitted (os error 1)\n"
        ));
        assert!(out.contains("    profile harness \"/home/u/.cargo/bin/harness\" flags=(unconfined) {\n      userns,\n"), "{out}");
        assert!(out.contains("    sudo apparmor_parser -r /etc/apparmor.d/harness\n"));
        assert!(
            out.contains("    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0\n")
        );
        assert!(out.contains("    echo 'kernel.apparmor_restrict_unprivileged_userns = 0' | sudo tee /etc/sysctl.d/60-harness-userns.conf\n"));
        assert!(out.contains("Quarantined files go to /home/u/.local/share/harness/quarantine"));
        assert!(out.contains("sandbox.linux_git_protection = \"best-effort\""));
        assert!(out.ends_with("harness does not change any of these settings itself.\n"));
        assert!(!out.contains("could not tell"));
    }

    #[test]
    fn other_causes_get_their_own_fixes() {
        let out = render(&basic(Host {
            userns_clone: Some("0".into()),
            max_user_namespaces: Some("0".into()),
            container: true,
            ..Host::default()
        }));
        assert!(
            out.contains("    sudo sysctl -w kernel.unprivileged_userns_clone=1\n"),
            "{out}"
        );
        assert!(
            out.contains("    sudo sysctl -w user.max_user_namespaces=10000\n"),
            "{out}"
        );
        assert!(out.contains("--security-opt seccomp=unconfined"), "{out}");
        assert!(!out.contains("apparmor_parser"));
    }

    #[test]
    fn an_unknown_cause_says_where_to_look() {
        let out = render(&basic(Host::default()));
        assert!(
            out.contains("could not tell what blocks them here"),
            "{out}"
        );
    }

    #[test]
    fn a_locked_securebit_gets_its_own_fix() {
        let mut facts = basic(Host::default());
        facts.protection = Some(GitProtection::Basic {
            reason: "prctl(PR_SET_SECUREBITS) failed: a securebit is locked (SECBIT_NOROOT_LOCKED)"
                .into(),
        });
        let out = render(&facts);
        assert!(out.contains("A locked securebit is refusing"), "{out}");
        assert!(!out.contains("could not tell"), "{out}");
    }

    #[test]
    fn a_path_that_cannot_go_into_a_profile_is_not_printed_into_one() {
        let mut facts = basic(Host {
            apparmor_restrict: Some("1".into()),
            ..Host::default()
        });
        facts.exe = Some(PathBuf::from("/tmp/evil\"\u{1b}[31m/harness"));
        let out = render(&facts);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains("profile harness \""), "{out}");
        assert!(
            out.contains("cannot be written into an AppArmor profile"),
            "{out}"
        );
    }

    #[test]
    fn a_reason_with_control_characters_is_escaped() {
        let mut facts = basic(Host::default());
        facts.protection = Some(GitProtection::Basic {
            reason: "opening /w/\u{1b}]0;x\u{7} failed".into(),
        });
        let out = render(&facts);
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
    }

    #[test]
    fn the_full_tier_and_macos_need_no_fix() {
        let mut facts = basic(Host {
            apparmor_restrict: Some("1".into()),
            ..Host::default()
        });
        facts.protection = Some(GitProtection::Full);
        let out = render(&facts);
        assert!(
            out.starts_with(
                "Sandbox: landlock+seccomp\nGit metadata protection: full tier (read-only mounts in a user namespace)\n"
            ),
            "{out}"
        );
        // The full tier now also snapshots and restores every protected file (Task 8 R2-I1), so
        // it depends on the same subreaper, watcher and pidfd machinery as the basic tier.
        assert!(out.contains("measured during a session"), "{out}");
        assert!(
            out.contains("Daemons a command starts (gpg-agent, git credential-cache"),
            "{out}"
        );
        assert!(!out.contains("apparmor_parser"), "{out}");
        assert!(!out.contains("To get the full tier"), "{out}");
        facts.linux = false;
        facts.mechanism = Some("seatbelt");
        assert_eq!(
            render(&facts),
            "Sandbox: seatbelt\nGit metadata protection: full (writes to git hooks and config fail inside the sandbox)\n"
        );
    }

    #[test]
    fn no_sandbox_says_why() {
        let facts = Facts {
            linux: true,
            unavailable: Some("Landlock is not enabled in this kernel".into()),
            disabled: true,
            ..Facts::default()
        };
        assert_eq!(
            render(&facts),
            "Sandbox: none (Landlock is not enabled in this kernel); every shell command will need approval\nHARNESS_SANDBOX=none is set, so harness runs without the sandbox and asks before every shell command.\n"
        );
    }

    #[test]
    fn required_protection_is_described() {
        let mut facts = basic(Host::default());
        facts.setting = LinuxGitProtection::Required;
        assert!(
            render(&facts).contains("\"required\": every shell command asks first in this tier.")
        );
    }

    #[test]
    fn the_subreaper_and_watcher_and_pidfd_facts_are_reported() {
        let mut facts = basic(Host::default());
        facts.subreaper = Subreaper::Refused;
        facts.watcher = WatcherStatus {
            failures: 2,
            last_error: Some("Too many open files (os error 24)".into()),
        };
        let out = render(&facts);
        assert!(
            out.contains("The kernel refused to make harness a subreaper"),
            "{out}"
        );
        assert!(out.contains("could not start 2 time(s) here"), "{out}");
        assert!(out.contains("Too many open files (os error 24)"), "{out}");
        assert!(out.contains("fs.inotify.max_user_instances"), "{out}");
    }

    #[test]
    fn a_fresh_process_labels_what_it_has_not_measured_yet() {
        let out = render(&basic(Host::default()));
        assert!(out.contains("measured during a session"), "{out}");
        assert!(
            out.contains("The watcher that checks git metadata as it changes while a command runs"),
            "{out}"
        );
    }
}
