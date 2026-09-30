//! Approvals at the terminal. The agent asks through [`ChannelApprover`]; the session shows the
//! request, with a diff for a file change, until the user answers: once, for the rest of the
//! session, or no, with a reason for the model if they like.

use std::{
    cell::{Cell, RefCell},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use harness_core::{
    agent::{ApprovalDecision, ApprovalKind, ApprovalRequest, Approver},
    permission::resolve_path,
    redact::Redactor,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    text::{Line, Span},
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::{
    diff,
    style::Theme,
    text::{lines, reveal, sanitize, wrap},
};

/// Files larger than this are not read for a diff.
const MAX_DIFF_FILE: u64 = 1024 * 1024;

/// How long reading a file and diffing it for a prompt may take; the prompt shows without the
/// diff after that.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the user must pause before a prompt takes keys: it takes none until this long after
/// it was first drawn, and each key before then moves that to this long after the key. A user
/// typing a message as a prompt appears keeps typing until they notice it; their keys go to the
/// input, however long they type.
pub const ARMING_DELAY: Duration = Duration::from_millis(500);

/// When a prompt starts to take keys: once [`ARMING_DELAY`] has passed since it was first drawn
/// and since the last key typed for the input meanwhile.
#[derive(Debug, Default, Clone, Copy)]
pub struct Arming {
    drawn: Option<Instant>,
    /// The last key read before the prompt took keys, which went to the input.
    typed: Option<Instant>,
}

impl Arming {
    /// The prompt was drawn at `now`; only the first time counts.
    pub fn drawn(&mut self, now: Instant) {
        self.drawn.get_or_insert(now);
    }

    /// A key read at `now`, before the prompt took keys, went to the input: the prompt waits for
    /// a pause after it.
    pub fn typed(&mut self, now: Instant) {
        self.typed = Some(self.typed.map_or(now, |typed| typed.max(now)));
    }

    /// Whether typing went to the input while the prompt waited.
    pub fn typed_past(&self) -> bool {
        self.typed.is_some()
    }

    /// When keys start to answer the prompt: `None` until it is drawn.
    pub fn armed_at(&self) -> Option<Instant> {
        let drawn = self.drawn?;
        let quiet_from = self.typed.map_or(drawn, |typed| typed.max(drawn));
        Some(quiet_from + ARMING_DELAY)
    }

    /// Whether a key read at `now` answers the prompt.
    pub fn armed(&self, now: Instant) -> bool {
        self.armed_at().is_some_and(|at| now >= at)
    }
}

/// Where the user's answer goes.
pub type Reply = oneshot::Sender<ApprovalDecision>;
/// The approvals the agent asks for, in order.
pub type Requests = mpsc::UnboundedReceiver<(ApprovalRequest, Reply)>;

/// Asks the session's user: sends each request to the terminal and waits for the answer. If the
/// session has gone, the action is denied.
pub struct ChannelApprover {
    requests: mpsc::UnboundedSender<(ApprovalRequest, Reply)>,
}

impl ChannelApprover {
    /// The approver for the agent, and the requests for the session.
    pub fn new() -> (Arc<ChannelApprover>, Requests) {
        let (requests, receiver) = mpsc::unbounded_channel();
        (Arc::new(ChannelApprover { requests }), receiver)
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let (reply, answer) = oneshot::channel();
        if self.requests.send((request.clone(), reply)).is_err() {
            return ApprovalDecision::Deny { feedback: None };
        }
        answer
            .await
            .unwrap_or(ApprovalDecision::Deny { feedback: None })
    }
}

/// What a key did to the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answered {
    /// The user decided.
    Decided(ApprovalDecision),
    /// No, and stop the turn.
    Interrupt,
}

/// An approval waiting for the user's answer.
pub struct Prompt {
    request: ApprovalRequest,
    reply: Option<Reply>,
    /// What would run or change: the command, or the file's diff.
    body: Vec<Line<'static>>,
    /// `body` wrapped to the width it was last drawn at, and that width.
    wrapped: RefCell<Option<(usize, Vec<Line<'static>>)>>,
    /// The first row of the wrapped body shown.
    scroll: usize,
    /// How many rows of the body the last render had room for, and how many it has, for paging.
    room: Cell<usize>,
    rows: Cell<usize>,
    /// While the user types why they deny.
    feedback: Option<String>,
}

impl Prompt {
    /// The prompt for `request`, showing `body` (from [`body`]) under the reason.
    pub fn new(request: ApprovalRequest, reply: Reply, mut body: Vec<Line<'static>>) -> Prompt {
        reveal_all(&mut body);
        Prompt {
            request,
            reply: Some(reply),
            body,
            wrapped: RefCell::new(None),
            scroll: 0,
            room: Cell::new(10),
            rows: Cell::new(0),
            feedback: None,
        }
    }

    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Replaces the secrets `redactor` knows in what the prompt shows: the reason, and the
    /// command or the file's diff, which is read from the file as it is on disk.
    pub fn redact(&mut self, redactor: &Redactor) {
        self.request.reason = redactor.redact(&self.request.reason);
        for line in &mut self.body {
            for span in &mut line.spans {
                let redacted = redactor.redact(&span.content);
                if redacted != span.content {
                    span.content = redacted.into();
                }
            }
        }
        self.wrapped.replace(None);
    }

    /// Sends `decision` to the agent.
    pub fn answer(&mut self, decision: ApprovalDecision) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(decision);
        }
    }

    /// Takes a paste into the reason the user is typing; `false` when they are not typing one.
    pub fn paste(&mut self, text: &str) -> bool {
        match &mut self.feedback {
            Some(feedback) => {
                feedback.push_str(text);
                true
            }
            None => false,
        }
    }

    /// Handles a key. Only `y`, `a` (where offered), `n` and Esc answer, and the letters only
    /// without Ctrl or Alt: Enter, the new-line keys and readline's keys were typed for the input.
    pub fn key(&mut self, key: KeyEvent) -> Option<Answered> {
        let rows = self.room.get();
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if let Some(text) = &mut self.feedback {
            match key.code {
                KeyCode::Enter => {
                    let text = text.trim().to_string();
                    return Some(Answered::Decided(ApprovalDecision::Deny {
                        feedback: (!text.is_empty()).then_some(text),
                    }));
                }
                KeyCode::Esc => self.feedback = None,
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char(c) if plain => text.push(c),
                _ => {}
            }
            return None;
        }
        let last = self.rows.get().saturating_sub(rows);
        match key.code {
            KeyCode::Char('y') if plain => {
                return Some(Answered::Decided(ApprovalDecision::Approve));
            }
            KeyCode::Char('a') if plain && self.request.kind == ApprovalKind::Action => {
                return Some(Answered::Decided(ApprovalDecision::ApproveForSession));
            }
            KeyCode::Char('n') if plain => self.feedback = Some(String::new()),
            KeyCode::Esc => return Some(Answered::Interrupt),
            KeyCode::Down => self.scroll = (self.scroll + 1).min(last),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.scroll = (self.scroll + rows.max(1)).min(last)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(rows.max(1)),
            _ => {}
        }
        None
    }

    /// The prompt as at most `rows` lines `width` columns wide. The body scrolls by row, so
    /// that a line longer than the prompt can be read to its end; whenever a row is hidden, a
    /// line says which rows show.
    pub fn render(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut head = wrap(
            &Line::from(vec![
                Span::styled("approve? ", theme.warning()),
                Span::styled(
                    reveal(&sanitize(&self.request.reason).replace('\n', " ")),
                    theme.bold(),
                ),
            ]),
            width,
            &[],
            &[Span::raw("  ")],
        );
        let mut foot = Vec::new();
        match &self.feedback {
            Some(text) => foot.push(Line::from(vec![
                Span::styled(
                    "tell the model why (Enter to send, Esc to go back): ",
                    theme.dim(),
                ),
                Span::raw(sanitize(text)),
            ])),
            None => {
                let mut keys = vec![Span::styled("[y] ", theme.accent()), Span::raw("yes  ")];
                if self.request.kind == ApprovalKind::Action {
                    keys.push(Span::styled("[a] ", theme.accent()));
                    keys.push(Span::raw("yes, for this session  "));
                }
                keys.push(Span::styled("[n] ", theme.accent()));
                keys.push(Span::raw("no  "));
                keys.push(Span::styled("[Esc] ", theme.accent()));
                keys.push(Span::raw("no, and stop"));
                foot.extend(wrap(&Line::from(keys), width, &[], &[]));
            }
        }
        // A reason too long for the screen is cut, so the keys and some of the body show.
        let keep = rows.saturating_sub(foot.len() + 2).max(1);
        if head.len() > keep {
            head.truncate(keep);
            if let Some(last) = head.last_mut() {
                last.spans.push(Span::styled(" …", theme.dim()));
            }
        }
        let room = rows.saturating_sub(head.len() + foot.len() + 1).max(1);
        self.room.set(room);
        let mut wrapped = self.wrapped.borrow_mut();
        if wrapped.as_ref().is_none_or(|(at, _)| *at != width) {
            let body = self
                .body
                .iter()
                .flat_map(|line| wrap(line, width, &[Span::raw("  ")], &[Span::raw("  ")]))
                .collect();
            *wrapped = Some((width, body));
        }
        let body = wrapped
            .as_ref()
            .map(|(_, body)| body.as_slice())
            .unwrap_or_default();
        self.rows.set(body.len());
        // A scroll left over from a narrower width stops at the last page.
        let scroll = self.scroll.min(body.len().saturating_sub(room));
        head.extend(body.iter().skip(scroll).take(room).cloned());
        if body.len() > room {
            let end = (scroll + room).min(body.len());
            head.push(Line::from(Span::styled(
                format!(
                    "  rows {}-{end} of {}: Up, Down, PgUp and PgDn scroll",
                    scroll + 1,
                    body.len()
                ),
                theme.dim(),
            )));
        }
        head.extend(foot);
        head
    }

    /// Lines for the scrollback saying how the user answered.
    pub fn outcome(&self, answered: &Answered, theme: &Theme) -> Line<'static> {
        let reason = reveal(&sanitize(&self.request.reason).replace('\n', " "));
        let (mark, text) = match answered {
            Answered::Decided(ApprovalDecision::Approve) => ("✓", format!("approved: {reason}")),
            Answered::Decided(ApprovalDecision::ApproveForSession)
                if self.request.kept_for_session =>
            {
                ("✓", format!("approved for this session: {reason}"))
            }
            // The policy asks about it every time, so the approval applies once.
            Answered::Decided(ApprovalDecision::ApproveForSession) => {
                ("✓", format!("approved once: {reason}"))
            }
            Answered::Decided(ApprovalDecision::Deny {
                feedback: Some(why),
            }) => ("✗", format!("denied: {reason} ({})", sanitize(why))),
            Answered::Decided(_) => ("✗", format!("denied: {reason}")),
            Answered::Interrupt => ("✗", format!("denied, and stopped: {reason}")),
        };
        Line::from(Span::styled(format!("{mark} {text}"), theme.dim()))
    }
}

/// What the prompt for `request` shows under the reason, as [`body`] makes it, made off the
/// async runtime: reading the file can block (a slow file system), and diffing a large file takes
/// a while. After [`BODY_TIMEOUT`], a note says there is no diff.
pub async fn prepare_body(
    request: &ApprovalRequest,
    arguments: Option<Value>,
    workspace: PathBuf,
    theme: Theme,
) -> Vec<Line<'static>> {
    let for_body = request.clone();
    let made = within(BODY_TIMEOUT, move || {
        body(&for_body, arguments.as_ref(), &workspace, &theme)
    });
    made.await.unwrap_or_else(|| {
        lines(
            "reading the file for a diff took too long, so no diff is shown",
            theme.dim(),
        )
    })
}

/// What the prompt shows under the reason: the command, or the file's change as a diff.
pub fn body(
    request: &ApprovalRequest,
    arguments: Option<&Value>,
    workspace: &Path,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let text = |key: &str| {
        arguments
            .and_then(|a| a[key].as_str())
            .unwrap_or_default()
            .to_string()
    };
    match request.tool.as_str() {
        "write" | "edit" => {
            let path = text("path");
            let target = resolve_path(workspace, Path::new(&path));
            let old = match read_small(&target) {
                Ok(old) => old,
                // No diff, but still what would be written, or what the edit replaces.
                Err(why) => {
                    let mut out = lines(&format!("{path}: {why}"), theme.dim());
                    if request.tool == "write" {
                        out.extend(diff::unified("", &text("content"), 3, theme));
                    } else {
                        out.extend(diff::unified(
                            &text("old_string"),
                            &text("new_string"),
                            3,
                            theme,
                        ));
                    }
                    return out;
                }
            };
            let new = if request.tool == "write" {
                text("content")
            } else {
                let (from, to) = (text("old_string"), text("new_string"));
                let all = arguments.is_some_and(|a| a["replace_all"].as_bool() == Some(true));
                match &old {
                    Some(old) if !from.is_empty() && old.contains(&from) => {
                        if all {
                            old.replace(&from, &to)
                        } else {
                            old.replacen(&from, &to, 1)
                        }
                    }
                    // What the model sent, when it does not match the file.
                    _ => {
                        let mut out = lines(
                            &format!("{path}: the text to replace is not in the file"),
                            theme.dim(),
                        );
                        out.extend(diff::unified(&from, &to, 3, theme));
                        return out;
                    }
                }
            };
            let old = old.unwrap_or_default();
            let (added, removed) = diff::counts(&old, &new);
            let mut out = vec![Line::from(Span::styled(
                format!("{} {} (+{added} -{removed})", request.tool, sanitize(&path)),
                theme.bold(),
            ))];
            out.extend(diff::unified(&old, &new, 3, theme));
            out
        }
        "bash" => text("command")
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let marker = if i == 0 { "$ " } else { "  " };
                Line::from(vec![
                    Span::styled(marker, theme.dim()),
                    Span::raw(sanitize(l)),
                ])
            })
            .collect(),
        _ => match arguments {
            Some(args) => lines(&args.to_string(), theme.dim()),
            None => Vec::new(),
        },
    }
}

/// `work`'s result, from a thread off the async runtime; `None` once `limit` has passed.
async fn within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    tokio::time::timeout(limit, tokio::task::spawn_blocking(work))
        .await
        .ok()?
        .ok()
}

/// Shows what is invisible in every span of `body`, which is one line each.
fn reveal_all(body: &mut [Line<'static>]) {
    for line in body {
        for span in &mut line.spans {
            let shown = reveal(&span.content);
            if shown != span.content {
                span.content = shown.into();
            }
        }
    }
}

/// The text of the file at `path`: `None` when it does not exist; an error saying why when it
/// is too large or not text.
fn read_small(path: &Path) -> Result<Option<String>, String> {
    // Opening waits for no writer (a FIFO) and makes no terminal harness's own; what was opened is
    // then checked through its descriptor, and only a regular file is read.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read it for a diff ({e})")),
    };
    match file.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err("not a regular file, so no diff is shown".into()),
        Err(e) => return Err(format!("cannot read it for a diff ({e})")),
    }
    let mut bytes = Vec::new();
    file.take(MAX_DIFF_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read it for a diff ({e})"))?;
    if bytes.len() as u64 > MAX_DIFF_FILE {
        return Err("too large to show a diff".into());
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "not text, so no diff is shown".into())
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use super::*;

    // Review D I1, probe 4: a FIFO opened for reading waits for a writer, and a read of a terminal
    // never ends. Only a regular file is read.
    #[test]
    fn a_fifo_is_not_read_and_nothing_waits_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let (tx, rx) = mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || tx.send(read_small(&path)));
        let read = rx.recv_timeout(Duration::from_secs(2));
        if read.is_err() {
            // Lets the stuck read go before failing.
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
        }
        let read = read.expect("reading the FIFO for a diff blocked");
        assert_eq!(read, Err("not a regular file, so no diff is shown".into()));
    }

    // Review D I1: work that takes too long is given up on, so the prompt shows without it.
    #[tokio::test]
    async fn work_off_the_ui_is_given_up_on_after_its_time() {
        assert_eq!(within(Duration::from_secs(5), || 7).await, Some(7));
        let slow = within(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(500));
            7
        });
        assert_eq!(slow.await, None);
    }
}
