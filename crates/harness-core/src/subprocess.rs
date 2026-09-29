//! Running helper programs (such as `git`) with a time limit.

use std::{
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use nix::{
    sys::{
        signal::{Signal, killpg},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::Pid,
};

/// How long the output is still waited for once the command's process group was killed.
const AFTER_KILL: Duration = Duration::from_secs(1);

/// Runs `command` with stdin closed and stdout and stderr captured. Returns `Ok(None)` when it
/// was still running after `timeout`, or when it exited but a process it started still held its
/// output open then; everything in its process group has been killed in that case.
///
/// The command leads its own process group, so the timeout reaches what it started (a shell that
/// does not `exec` its last command leaves that command holding the pipes). It stays in harness's
/// session, as harness's helpers must: see `harness_sandbox::procs`.
pub fn output_within(command: &mut Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let group = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
    // Read both pipes on their own threads, so a child that fills a pipe cannot stall.
    let out = read_all(child.stdout.take().expect("stdout is piped"));
    let err = read_all(child.stderr.take().expect("stderr is piped"));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = killpg(group, Signal::SIGKILL);
                break None;
            }
            Err(e) => {
                let _ = killpg(group, Signal::SIGKILL);
                let _ = child.wait();
                return Err(e);
            }
        }
    };
    // The pipes close once nothing holds them any more. Something the command started may still
    // hold them after it exited: that counts as running, until the deadline.
    let mut timed_out = status.is_none();
    if !timed_out && !finished_by(&[&out, &err], deadline) {
        let _ = killpg(group, Signal::SIGKILL);
        timed_out = true;
    }
    let reaped = child.wait().is_ok();
    // A process outside the group (one that made its own session) could still hold a pipe: its
    // reader is then left to finish on its own rather than waited for.
    let finished = finished_by(&[&out, &err], Instant::now() + AFTER_KILL);
    // Only once the command itself is reaped, so waiting on its group cannot take its status.
    if reaped {
        reap_group(group, timed_out);
    }
    if timed_out || !finished {
        return Ok(None);
    }
    Ok(status.map(|status| Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    }))
}

/// Reads `pipe` to its end on a thread of its own.
fn read_all(mut pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Whether all of `readers` finished by `deadline`.
fn finished_by(readers: &[&JoinHandle<Vec<u8>>], deadline: Instant) -> bool {
    loop {
        if readers.iter().all(|r| r.is_finished()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Reaps the members of `group` that are harness's own children: where harness is a child
/// subreaper (Linux git-metadata protection), what the command started is reparented to harness
/// when the command exits, and stays a zombie until reaped. After a kill, the group is watched
/// until it is empty, for [`AFTER_KILL`] at most. Only this group is waited for, never any child
/// (`-1`), which would take statuses std and tokio wait for.
fn reap_group(group: Pid, killed: bool) {
    // A group id of 1 or less would make this `waitpid(-1)` or worse.
    if group.as_raw() <= 1 {
        return;
    }
    let members = Pid::from_raw(-group.as_raw());
    let deadline = Instant::now() + AFTER_KILL;
    loop {
        while let Ok(status) = waitpid(members, Some(WaitPidFlag::WNOHANG)) {
            if status == WaitStatus::StillAlive {
                break;
            }
        }
        if !killed || killpg(group, None).is_err() || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_collected() {
        let out = output_within(
            Command::new("/bin/sh").args(["-c", "echo out; echo err >&2"]),
            Duration::from_secs(10),
        )
        .unwrap()
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }

    #[test]
    fn a_command_past_its_timeout_is_killed() {
        let start = Instant::now();
        let out = output_within(
            Command::new("/bin/sh").args(["-c", "sleep 30"]),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(out.is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// A shell that does not `exec` its last command (dash, `/bin/sh` on Ubuntu, never does, and
    /// no shell does when another command follows) leaves the command's own child holding the
    /// pipes: it must be killed too, or reading the output waits for it.
    #[test]
    fn a_grandchild_holding_the_pipes_is_killed_too() {
        let start = Instant::now();
        let out = output_within(
            Command::new("/bin/sh").args(["-c", "sleep 30; exit 0"]),
            Duration::from_millis(200),
        )
        .unwrap();
        assert!(out.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
    }

    /// A command that exits in time but leaves a process running that holds its output open has
    /// not finished either: that process is ended once the time is up.
    #[test]
    fn a_background_process_holding_the_pipes_is_ended_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; exit 0", pid_file.display());
        let start = Instant::now();
        let out = output_within(
            Command::new("/bin/sh").args(["-c", &script]),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(out.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // It was killed: gone, or a zombie until whoever it was reparented to reaps it.
        let alive = || {
            let out = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            !stat.is_empty() && !stat.starts_with('Z')
        };
        let gone = Instant::now();
        while alive() {
            assert!(
                gone.elapsed() < Duration::from_secs(5),
                "the background process is still running"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A command that finishes in time is not affected by what the timeout does.
    #[test]
    fn a_background_process_that_does_not_hold_the_pipes_is_left_alone() {
        let out = output_within(
            Command::new("/bin/sh").args(["-c", "sleep 1 >/dev/null 2>&1 & echo done"]),
            Duration::from_secs(10),
        )
        .unwrap()
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"done\n");
    }

    #[test]
    fn large_output_does_not_stall_the_command() {
        let out = output_within(
            Command::new("/bin/sh").args(["-c", "head -c 1000000 /dev/zero"]),
            Duration::from_secs(10),
        )
        .unwrap()
        .unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
    }
}
