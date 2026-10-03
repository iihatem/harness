//! Verification gates: the settings (`[gates]`), and, in later tasks, how the agent runs them.

/// The longest a gate command may be given: what the bash tool allows any command.
pub const MAX_TIMEOUT_S: u64 = 600;

/// The effective `[gates]` settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gates {
    /// A lint command run after each successful edit.
    pub after_edit: Option<String>,
    /// A test command run when a turn that changed files ends.
    pub test: Option<String>,
    pub timeout_s: u64,
    /// How many times a failed test may continue the turn.
    pub max_retries: u32,
    /// How many lines of a failing command's output the model gets.
    pub output_tail_lines: usize,
}

impl Default for Gates {
    fn default() -> Self {
        Gates {
            after_edit: None,
            test: None,
            timeout_s: 300,
            max_retries: 3,
            output_tail_lines: 60,
        }
    }
}

impl Gates {
    /// Whether any gate command is set.
    pub fn is_configured(&self) -> bool {
        self.after_edit.is_some() || self.test.is_some()
    }
}
