//! Secrets harness knows, kept out of everything it writes: session files, tool-output files, the
//! debug log, NDJSON and what it prints. They are the API keys and tokens it holds and the values
//! of environment variables whose names mark them as secrets. What the model is sent is left as
//! it is, so a file it reads and writes back keeps its real contents.

use std::{
    ffi::OsStr,
    ops::Range,
    sync::{Arc, RwLock},
};

use serde_json::Value;

use crate::event::AgentEvent;

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

/// The fields of harness's events and session entries whose values are harness's own words for
/// what the record is (`"type": "text_delta"`, `"role": "assistant"`, an error's `kind`, a
/// rewind's `scope`), never data. [`Redactor::redact_value`] leaves them alone, so that a secret
/// that happens to be one of those words does not break the record.
pub const WORD_FIELDS: [&str; 4] = ["type", "role", "kind", "scope"];

/// What an event that could not be shown without a secret is replaced with.
const LEFT_OUT: &str = "an event was left out because it could not be shown without a secret";

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

    /// `text`, a whole message as it is recorded (a reply of the model), redacted: each secret in
    /// it, and an edge of it that is part of a secret. A reply cut off at the output limit in the
    /// middle of a secret ends with the secret's start, and the reply that continues it starts
    /// with the rest: an end of the message that a secret starts with, or a start that a secret
    /// ends with, is replaced too when it is at least [`MIN_SECRET_LEN`] bytes long.
    pub fn redact_message(&self, text: &str) -> String {
        let lead = self.lead_len(text);
        let rest = &text[lead..];
        let tail = self.tail_from(rest);
        edged(lead > 0, self.redact(&rest[..tail]), tail < rest.len())
    }

    /// Where the secrets are in `text`: the byte range of each occurrence, overlapping ones
    /// included, in order.
    pub fn occurrences(&self, text: &str) -> Vec<Range<usize>> {
        occurrences(&self.secrets.read().expect("secrets lock"), text)
    }

    /// The secrets in `text` near byte `at`, which [`occurrences`](Self::occurrences) finds
    /// within the longest secret's length of it: every secret that runs through `at` among them.
    /// Only that much of `text` is searched, however long it is.
    pub fn occurrences_around(&self, text: &str, at: usize) -> Vec<Range<usize>> {
        let secrets = self.secrets.read().expect("secrets lock");
        let longest = secrets.first().map_or(0, String::len);
        let start = floor_boundary(text, at.saturating_sub(longest));
        let end = ceil_boundary(text, at.saturating_add(longest).min(text.len()));
        occurrences(&secrets, &text[start..end])
            .into_iter()
            .map(|found| found.start + start..found.end + start)
            .collect()
    }

    /// `at`, a cut of `text`, moved back (`back`) or forward past each secret that runs through
    /// it, so that the cut runs through none: never by more than the longest secret's length,
    /// which a secret that overlaps itself (`00000000` in a run of zeros) would otherwise take
    /// the cut through the whole text.
    pub fn clear_cut(&self, text: &str, at: usize, back: bool) -> usize {
        let near = self.occurrences_around(text, at);
        let mut at = at;
        loop {
            let crossing = near
                .iter()
                .filter(|found| found.start < at && found.end > at);
            let next = if back {
                crossing.map(|found| found.start).min()
            } else {
                crossing.map(|found| found.end).max()
            };
            match next {
                Some(next) => at = next,
                None => return at,
            }
        }
    }

    /// How much of the start of `text` is the end of a secret: the longest start of at least
    /// [`MIN_SECRET_LEN`] bytes that a secret ends with (a whole secret is not an end of one), and
    /// any secret that starts in it and runs on past it. 0 when there is none.
    fn lead_len(&self, text: &str) -> usize {
        let lead = {
            let secrets = self.secrets.read().expect("secrets lock");
            let bytes = text.as_bytes();
            secrets
                .iter()
                .filter_map(|secret| {
                    let longest = (secret.len() - 1).min(text.len());
                    (MIN_SECRET_LEN..=longest).rev().find(|&k| {
                        text.is_char_boundary(k) && secret.as_bytes().ends_with(&bytes[..k])
                    })
                })
                .max()
                .unwrap_or(0)
        };
        if lead == 0 {
            return 0;
        }
        self.clear_cut(text, lead, false)
    }

    /// Where the end of `text` that is the start of a secret begins: the longest end of at least
    /// [`MIN_SECRET_LEN`] bytes that a secret starts with (a whole secret is not a start of one),
    /// moved back over any secret that runs into it. `text.len()` when there is none.
    fn tail_from(&self, text: &str) -> usize {
        if text.len() < MIN_SECRET_LEN {
            return text.len();
        }
        let from = {
            let secrets = self.secrets.read().expect("secrets lock");
            let bytes = text.as_bytes();
            let latest = text.len() - MIN_SECRET_LEN;
            secrets
                .iter()
                .filter_map(|secret| {
                    let earliest = (text.len() + 1).saturating_sub(secret.len());
                    (earliest..=latest).find(|&i| {
                        text.is_char_boundary(i) && secret.as_bytes().starts_with(&bytes[i..])
                    })
                })
                .min()
                .unwrap_or(text.len())
        };
        if from == text.len() {
            return from;
        }
        self.clear_cut(text, from, true)
    }

    /// Whether more text after `text`, the start of a message, could make it a longer end of a
    /// secret of at least [`MIN_SECRET_LEN`] bytes than it is.
    fn could_end_a_secret(&self, text: &str) -> bool {
        let secrets = self.secrets.read().expect("secrets lock");
        secrets.iter().any(|secret| {
            // Where in the secret such an end would start.
            let latest = secret
                .len()
                .saturating_sub(MIN_SECRET_LEN.max(text.len() + 1));
            (1..=latest).any(|i| secret.is_char_boundary(i) && secret[i..].starts_with(text))
        })
    }

    /// `text`, the end of a message whose start was shown already, redacted as
    /// [`redact_message`](Self::redact_message) redacts the end of one.
    fn redact_end(&self, text: &str) -> String {
        let tail = self.tail_from(text);
        edged(false, self.redact(&text[..tail]), tail < text.len())
    }

    /// A stream for text that arrives in pieces, such as a model's streamed reply: a secret split
    /// across pieces is replaced whole.
    pub fn stream(self: &Arc<Self>) -> StreamRedactor {
        StreamRedactor {
            redactor: self.clone(),
            pending: String::new(),
            started: false,
        }
    }

    /// Redacts each string in `value`, one of harness's own records (an event or a session
    /// entry) as JSON. Each string is matched on its own, so text that holds JSON, such as a tool
    /// call's arguments, is matched in the form the secret takes inside it. Object keys and the
    /// values of the [`WORD_FIELDS`] are left alone.
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.redact(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_value(item)),
            Value::Object(fields) => {
                for (name, field) in fields.iter_mut() {
                    if !(WORD_FIELDS.contains(&name.as_str()) && field.is_string()) {
                        self.redact_value(field);
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// `event` with each string in it redacted (see [`redact_value`](Self::redact_value)). The
    /// text of a delta is matched on its own: [`EventRedactor`] also finds secrets split across
    /// deltas.
    pub fn redact_event(&self, event: &AgentEvent) -> AgentEvent {
        match event {
            // They hold nothing but harness's words for how the turn went.
            AgentEvent::TurnStarted | AgentEvent::TurnFinished { .. } => return event.clone(),
            // A reply is a whole message: its edges can be parts of secrets.
            AgentEvent::AssistantMessage { content, model } => {
                return AgentEvent::AssistantMessage {
                    content: self.redact_message(content),
                    model: self.redact(model),
                };
            }
            _ => {}
        }
        let mut value = serde_json::to_value(event).expect("events serialize");
        self.redact_value(&mut value);
        serde_json::from_value(value).unwrap_or_else(|_| AgentEvent::Warning {
            message: LEFT_OUT.into(),
        })
    }

    /// Where a stream may cut `text` without splitting a secret: before the longest end of
    /// `text` that could still become a secret as more text arrives, and before any secret that
    /// the cut would otherwise run through.
    fn hold_from(&self, text: &str) -> usize {
        let secrets = self.secrets.read().expect("secrets lock");
        let bytes = text.as_bytes();
        let mut from = text.len();
        for secret in secrets.iter() {
            // An end as long as the secret is either the secret, or cannot become it.
            let earliest = text.len().saturating_sub(secret.len() - 1);
            if let Some(start) =
                (earliest..from).find(|&i| secret.as_bytes().starts_with(&bytes[i..]))
            {
                from = start;
            }
        }
        // A secret that is already whole is held back with what follows, never cut.
        while let Some(start) = secrets
            .iter()
            .flat_map(|secret| {
                text.match_indices(secret.as_str())
                    .map(|(i, found)| i..i + found.len())
            })
            .filter(|found| found.start < from && found.end > from)
            .map(|found| found.start)
            .min()
        {
            from = start;
        }
        from
    }
}

/// Redacts text that arrives in pieces (see [`Redactor::stream`]), a message: what it shows adds
/// up to [`Redactor::redact_message`] of the whole. It holds back only the end of the text so far
/// that could still become a secret, and the start of the text while it could still be the end
/// of one, and shows everything else, redacted.
#[derive(Debug)]
pub struct StreamRedactor {
    redactor: Arc<Redactor>,
    /// Text received and not yet shown.
    pending: String,
    /// Whether the start of the text has been shown.
    started: bool,
}

impl StreamRedactor {
    /// Adds `piece`, and returns what can be shown now, redacted. It can be empty.
    pub fn push(&mut self, piece: &str) -> String {
        self.pending.push_str(piece);
        let mut shown = String::new();
        if !self.started {
            if self.redactor.could_end_a_secret(&self.pending) {
                return shown;
            }
            let lead = self.redactor.lead_len(&self.pending);
            // A secret that may start within that end of one is waited for too.
            if lead > 0 && self.redactor.hold_from(&self.pending) < lead {
                return shown;
            }
            if lead > 0 {
                shown.push_str(REDACTED);
                self.pending.drain(..lead);
            }
            self.started = true;
        }
        let held = self.redactor.hold_from(&self.pending);
        let held = self.pending.split_off(held);
        let ready = std::mem::replace(&mut self.pending, held);
        shown.push_str(&self.redactor.redact(&ready));
        shown
    }

    /// Everything held back, redacted: the text, a message, has ended. The stream then starts
    /// afresh, with the next message.
    pub fn finish(&mut self) -> String {
        let text = std::mem::take(&mut self.pending);
        if std::mem::replace(&mut self.started, false) {
            self.redactor.redact_end(&text)
        } else {
            self.redactor.redact_message(&text)
        }
    }
}

/// Redacts a turn's events for a frontend to show or write: the strings of each event, and the
/// text of the deltas across events, through one stream for the reply's text and one for its
/// reasoning. What the streams hold back comes out, as deltas, before the next event of another
/// kind (other than usage, which can arrive while a reply streams) and at
/// [`finish`](Self::finish).
#[derive(Debug)]
pub struct EventRedactor {
    redactor: Arc<Redactor>,
    text: StreamRedactor,
    reasoning: StreamRedactor,
}

impl EventRedactor {
    pub fn new(redactor: Arc<Redactor>) -> EventRedactor {
        EventRedactor {
            text: redactor.stream(),
            reasoning: redactor.stream(),
            redactor,
        }
    }

    /// The events to show for `event`, redacted: none while all of a delta's text is held back.
    pub fn push(&mut self, event: AgentEvent) -> Vec<AgentEvent> {
        match event {
            AgentEvent::TextDelta { text } => {
                delta(self.text.push(&text), |text| AgentEvent::TextDelta { text })
            }
            AgentEvent::ReasoningDelta { text } => delta(self.reasoning.push(&text), |text| {
                AgentEvent::ReasoningDelta { text }
            }),
            // Some servers report usage with every chunk: it does not end the reply.
            usage @ AgentEvent::Usage { .. } => vec![self.redactor.redact_event(&usage)],
            other => {
                let mut shown = self.finish();
                shown.push(self.redactor.redact_event(&other));
                shown
            }
        }
    }

    /// What the streams still hold back, as deltas: the events have ended, or an event of
    /// another kind follows.
    pub fn finish(&mut self) -> Vec<AgentEvent> {
        let mut shown = delta(self.reasoning.finish(), |text| AgentEvent::ReasoningDelta {
            text,
        });
        shown.extend(delta(self.text.finish(), |text| AgentEvent::TextDelta {
            text,
        }));
        shown
    }
}

/// Where `secrets` are in `text`: the byte range of each occurrence, overlapping ones included,
/// in order.
fn occurrences(secrets: &[String], text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    for secret in secrets {
        let mut from = 0;
        while let Some(i) = text[from..].find(secret.as_str()) {
            let start = from + i;
            found.push(start..start + secret.len());
            from = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    found.sort_by_key(|found| (found.start, found.end));
    found
}

/// `middle`, after [`REDACTED`] when `lead` and before it when `tail`: a message whose edges
/// were parts of secrets.
fn edged(lead: bool, middle: String, tail: bool) -> String {
    let mut text = String::new();
    if lead {
        text.push_str(REDACTED);
    }
    text.push_str(&middle);
    if tail {
        text.push_str(REDACTED);
    }
    text
}

/// The char boundary at or before `i` in `s`.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The char boundary at or after `i` in `s`.
fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// A delta of `text`, unless there is none.
fn delta(text: String, event: impl FnOnce(String) -> AgentEvent) -> Vec<AgentEvent> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![event(text)]
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
