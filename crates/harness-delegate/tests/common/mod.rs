#![allow(dead_code)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use harness_core::event::TurnEndReason;
use harness_delegate::{
    DelegateError,
    driver::{Out, Protocol},
    event::{ApprovalAnswer, DelegateEvent},
};
use nix::sys::signal::Signal;
use tokio::sync::mpsc::UnboundedReceiver;

pub const FAKE: &str = env!("CARGO_BIN_EXE_fake-delegate-child");

/// A scripted fake child: a wrapper script that answers `--version` and otherwise runs the fake
/// child binary with `script`, logging what it reads and its arguments and environment.
pub struct Fake {
    pub dir: tempfile::TempDir,
    pub binary: PathBuf,
    pub log: PathBuf,
    pub env_dump: PathBuf,
}

impl Fake {
    pub fn new(name: &str, version_line: &str, script: &str) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("script.txt");
        std::fs::write(&script_path, script).unwrap();
        let log = dir.path().join("log.txt");
        let env_dump = dir.path().join("env.txt");
        let binary = dir.path().join(name);
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version_line}'; exit 0; fi\n\
                 FAKE_CHILD_SCRIPT='{}' FAKE_CHILD_LOG='{}' FAKE_CHILD_ENVDUMP='{}' exec '{FAKE}' \"$@\"\n",
                script_path.display(),
                log.display(),
                env_dump.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake {
            dir,
            binary,
            log,
            env_dump,
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// What the child logged: `argv:` first, then each stdin line it read as `in: <line>`.
    pub fn log(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    pub fn stdin_lines(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter_map(|l| l.strip_prefix("in: ").map(String::from))
            .collect()
    }

    pub fn argv(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .find_map(|l| l.strip_prefix("argv: ").map(String::from))
            .map(|a| a.split('\u{1f}').map(String::from).collect())
            .unwrap_or_default()
    }

    pub fn env(&self) -> std::collections::BTreeMap<String, String> {
        std::fs::read_to_string(&self.env_dump)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

/// Every event up to and including the stream's end, within `limit`.
pub async fn collect(
    rx: &mut UnboundedReceiver<DelegateEvent>,
    limit: Duration,
) -> Vec<DelegateEvent> {
    let mut all = Vec::new();
    let end = tokio::time::Instant::now() + limit;
    loop {
        match tokio::time::timeout_at(end, rx.recv()).await {
            Ok(Some(event)) => all.push(event),
            Ok(None) => return all,
            Err(_) => panic!("the stream did not end within {limit:?}; got {all:#?}"),
        }
    }
}

pub fn texts(events: &[DelegateEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            DelegateEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

pub fn finished(events: &[DelegateEvent]) -> Option<TurnEndReason> {
    events.iter().rev().find_map(|e| match e {
        DelegateEvent::Finished { reason, .. } => Some(*reason),
        _ => None,
    })
}

/// A protocol of single words: every line is a text delta, `done` finishes, an interrupt writes
/// `INTERRUPT`, an answer writes `ANSWER <id>`.
#[derive(Default)]
pub struct Toy {
    finished: bool,
}

impl Protocol for Toy {
    fn start(&mut self) -> Out {
        Out::default()
    }

    fn on_line(&mut self, line: &str) -> Out {
        let mut out = Out::default();
        if line == "done" {
            self.finished = true;
            out.events.push(DelegateEvent::Finished {
                reason: TurnEndReason::Completed,
                detail: None,
            });
            out.finished = true;
            out.close_stdin = true;
        } else {
            out.events.push(DelegateEvent::TextDelta {
                text: line.to_string(),
            });
        }
        out
    }

    fn on_answer(&mut self, id: &str, _answer: ApprovalAnswer) -> Result<Out, DelegateError> {
        if id != "x" {
            return Err(DelegateError::UnknownApproval(id.to_string()));
        }
        Ok(Out {
            writes: vec![format!("ANSWER {id}")],
            ..Out::default()
        })
    }

    fn on_interrupt(&mut self) -> Out {
        Out {
            writes: vec!["INTERRUPT".into()],
            ..Out::default()
        }
    }

    fn escalation(&self) -> &'static [Signal] {
        &[Signal::SIGINT, Signal::SIGTERM, Signal::SIGKILL]
    }
}
