//! What `harness ask` prints on stderr before its agent starts: warnings about the configuration,
//! the sandbox, the model's context window, instruction and command files, and notes. They are
//! printed with the secrets harness knows redacted, and kept so that `--debug` logs them too.

use std::sync::Arc;

use harness_core::{event::AgentEvent, redact::Redactor};

use crate::term::terminal_safe;

pub struct Notices {
    redactor: Arc<Redactor>,
    /// What was printed, redacted, as the messages of warning events.
    kept: Vec<String>,
}

impl Notices {
    pub fn new(redactor: Arc<Redactor>) -> Notices {
        Notices {
            redactor,
            kept: Vec::new(),
        }
    }

    /// Prints `warning: <message>`, and keeps it.
    pub fn warn(&mut self, message: &str) {
        let message = self.redactor.redact(message);
        eprintln!("warning: {}", terminal_safe(&message));
        self.kept.push(message);
    }

    /// Prints `note: <message>`, and keeps it.
    pub fn note(&mut self, message: &str) {
        let message = self.redactor.redact(message);
        eprintln!("note: {}", terminal_safe(&message));
        self.kept.push(format!("note: {message}"));
    }

    /// Keeps `message`, a warning that was printed already.
    pub fn printed(&mut self, message: &str) {
        self.kept.push(self.redactor.redact(message));
    }

    /// What was kept, as warning events for the debug log.
    pub fn into_events(self) -> Vec<AgentEvent> {
        self.kept
            .into_iter()
            .map(|message| AgentEvent::Warning { message })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_are_kept_redacted() {
        let redactor = Arc::new(Redactor::default());
        redactor.add("sk-canary-0123456789");
        let mut notices = Notices::new(redactor);
        notices.printed("from the configuration");
        notices.warn("the key sk-canary-0123456789 is odd");
        notices.note("a note");
        assert_eq!(
            notices.into_events(),
            [
                "from the configuration",
                "the key [redacted] is odd",
                "note: a note"
            ]
            .map(|message| AgentEvent::Warning {
                message: message.into()
            })
        );
    }
}
