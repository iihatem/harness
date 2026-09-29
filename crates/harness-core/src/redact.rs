//! Secrets harness knows, kept out of everything it writes: session files, tool-output files, the
//! debug log, NDJSON and what it prints. They are the API keys and tokens it uses and the values
//! of environment variables whose names mark them as secrets. What the model is sent is left as
//! it is, so a file it reads and writes back keeps its real contents.

use std::{ffi::OsStr, sync::RwLock};

/// What a secret is replaced with.
pub const REDACTED: &str = "[redacted]";
/// Shorter values are not treated as secrets: they would match ordinary text.
pub const MIN_SECRET_LEN: usize = 8;
/// How the names of environment variables that hold secrets end, compared without regard to
/// case. `_PASS` and `_PWD` need the underscore, so that `COMPASS`, `PWD` and `OLDPWD` are not
/// secrets.
pub const SECRET_NAME_ENDINGS: [&str; 12] = [
    "KEY",
    "KEYS",
    "TOKEN",
    "TOKENS",
    "SECRET",
    "SECRETS",
    "PASSWORD",
    "PASSWORDS",
    "PASSPHRASE",
    "CREDENTIALS",
    "_PASS",
    "_PWD",
];

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

    /// Adds the values of the variables in `vars` whose names mark them as secrets (see
    /// [`SECRET_NAME_ENDINGS`]), and the password of any URL a variable holds
    /// (`scheme://user:password@host`). A name or value that is not UTF-8 is read lossily, as
    /// the bash tool reads what a command prints.
    pub fn add_env<N: AsRef<OsStr>, V: AsRef<OsStr>>(
        &self,
        vars: impl IntoIterator<Item = (N, V)>,
    ) {
        for (name, value) in vars {
            let name = name.as_ref().to_string_lossy().to_ascii_uppercase();
            let value = value.as_ref().to_string_lossy();
            if SECRET_NAME_ENDINGS
                .iter()
                .any(|ending| name.ends_with(ending))
            {
                self.add(&value);
            }
            for password in url_passwords(&value) {
                self.add(&password);
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

/// The passwords of the URLs in `value` (`scheme://user:password@host`), as written and, when
/// they differ, percent-decoded.
fn url_passwords(value: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = value;
    while let Some(at) = rest.find("://") {
        rest = &rest[at + 3..];
        let authority = &rest[..rest
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(rest.len())];
        let Some((userinfo, _host)) = authority.rsplit_once('@') else {
            continue;
        };
        let Some((_user, password)) = userinfo.split_once(':') else {
            continue;
        };
        let decoded = percent_decode(password);
        if decoded != password {
            found.push(decoded);
        }
        found.push(password.to_string());
    }
    found
}

/// `text` with each `%XX` replaced by the byte it stands for.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit))
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
