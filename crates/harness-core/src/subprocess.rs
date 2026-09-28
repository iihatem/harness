//! Running helper programs (such as `git`) with a time limit.

use std::{
    io::Read,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// Runs `command` with stdin closed and stdout and stderr captured. Returns `Ok(None)` when it
/// was still running after `timeout`, in which case it has been killed.
pub fn output_within(command: &mut Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Read both pipes on their own threads, so a child that fills a pipe cannot stall.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok(status.map(|status| Output {
        status,
        stdout,
        stderr,
    }))
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
