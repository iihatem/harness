//! Secrets harness knows, kept out of everything it writes: session files, tool-output files, the
//! debug log, NDJSON and what it prints. They are the API keys and tokens it uses and the values
//! of environment variables whose names mark them as secrets. What the model is sent is left as
//! it is, so a file it reads and writes back keeps its real contents.

use std::{ffi::OsStr, sync::RwLock};

/// What a secret is replaced with.
pub const REDACTED: &str = "[redacted]";
/// Shorter values are not treated as secrets: they would match ordinary text.
pub const MIN_SECRET_LEN: usize = 8;

/// The secrets to keep out of what harness writes. Shared, and added to as tokens are refreshed.
#[derive(Default)]
pub struct Redactor {
    secrets: RwLock<Vec<String>>,
}

/// Shows how many secrets it holds, never the secrets.
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.secrets.read().map(|s| s.len()).unwrap_or(0);
        write!(f, "Redactor({count} secrets)")
    }
}

impl Redactor {
    /// Adds `secret`, and the form it takes inside a JSON string, unless it is shorter than
    /// [`MIN_SECRET_LEN`].
    pub fn add(&self, secret: &str) {
        let secret = secret.trim();
        if secret.len() < MIN_SECRET_LEN {
            return;
        }
        let quoted = serde_json::to_string(secret).expect("a string serializes");
        let escaped = &quoted[1..quoted.len() - 1];
        let mut secrets = self.secrets.write().expect("secrets lock");
        for form in [secret, escaped] {
            if !secrets.iter().any(|s| s == form) {
                secrets.push(form.to_string());
            }
        }
        // Longest first, so that a secret containing another is replaced whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    }

    /// Adds the values of the variables in `vars` whose names end in `KEY`, `TOKEN`, `SECRET` or
    /// `PASSWORD`, in any case. A name or value that is not UTF-8 is read lossily, as the bash
    /// tool reads what a command prints.
    pub fn add_env<N: AsRef<OsStr>, V: AsRef<OsStr>>(
        &self,
        vars: impl IntoIterator<Item = (N, V)>,
    ) {
        for (name, value) in vars {
            let name = name.as_ref().to_string_lossy().to_ascii_uppercase();
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
            {
                self.add(&value.as_ref().to_string_lossy());
            }
        }
    }

    /// `text` with every secret replaced by [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        let secrets = self.secrets.read().expect("secrets lock");
        let mut text = text.to_string();
        for secret in secrets.iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), REDACTED);
            }
        }
        text
    }
}
