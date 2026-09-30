//! Approvals at the terminal. The agent asks through [`ChannelApprover`]; the session shows the
//! request, with a diff for a file change, until the user answers: once, for the rest of the
//! session, or no, with a reason for the model if they like.

use std::{
    cell::Cell,
    io::Read,
    path::Path,
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
    text::{lines, sanitize, wrap},
};

/// Files larger than this are not read for a diff.
const MAX_DIFF_FILE: u64 = 1024 * 1024;

/// How long after a prompt is first drawn its keys start to answer it. A key pressed before then
/// was typed for the input, before the user could have read the prompt.
pub const ARMING_DELAY: Duration = Duration::from_millis(300);

/// When a prompt starts to take keys: [`ARMING_DELAY`] after it was first drawn.
#[derive(Debug, Default, Clone, Copy)]
pub struct Arming {
    drawn: Option<Instant>,
}

impl Arming {
    /// The prompt was drawn at `now`; only the first time counts.
    pub fn drawn(&mut self, now: Instant) {
        self.drawn.get_or_insert(now);
    }

    /// When keys start to answer the prompt: `None` until it is drawn.
    pub fn armed_at(&self) -> Option<Instant> {
        self.drawn.map(|at| at + ARMING_DELAY)
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
    /// The first line of `body` shown.
    scroll: usize,
    /// How many lines of `body` the last render showed, for paging.
    room: Cell<usize>,
    /// While the user types why they deny.
    feedback: Option<String>,
}

impl Prompt {
    /// The prompt for `request`, whose tool call has `arguments` (when known), in `workspace`.
    pub fn new(
        request: ApprovalRequest,
        reply: Reply,
        arguments: Option<&Value>,
        workspace: &Path,
        theme: &Theme,
    ) -> Prompt {
        let body = body(&request, arguments, workspace, theme);
        Prompt {
            request,
            reply: Some(reply),
            body,
            scroll: 0,
            room: Cell::new(10),
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
    }

    /// Sends `decision` to the agent.
    pub fn answer(&mut self, decision: ApprovalDecision) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(decision);
        }
    }

    /// Whether the user is typing why they deny.
    pub fn typing_reason(&self) -> bool {
        self.feedback.is_some()
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
        let last = self.body.len().saturating_sub(rows);
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

    /// The prompt as at most `rows` lines `width` columns wide.
    pub fn render(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut head = wrap(
            &Line::from(vec![
                Span::styled("approve? ", theme.warning()),
                Span::styled(
                    sanitize(&self.request.reason).replace('\n', " "),
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
        let shown: Vec<Line<'static>> = self
            .body
            .iter()
            .skip(self.scroll)
            .take(room)
            .flat_map(|line| wrap(line, width, &[Span::raw("  ")], &[Span::raw("  ")]))
            .take(room)
            .collect();
        head.extend(shown);
        if self.body.len() > room {
            let end = (self.scroll + room).min(self.body.len());
            head.push(Line::from(Span::styled(
                format!(
                    "  lines {}-{end} of {}: Up, Down, PgUp and PgDn scroll",
                    self.scroll + 1,
                    self.body.len()
                ),
                theme.dim(),
            )));
        }
        head.extend(foot);
        head
    }

    /// Lines for the scrollback saying how the user answered.
    pub fn outcome(&self, answered: &Answered, theme: &Theme) -> Line<'static> {
        let reason = sanitize(&self.request.reason).replace('\n', " ");
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

/// What the prompt shows under the reason: the command, or the file's change as a diff.
fn body(
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
                Err(why) => {
                    return lines(&format!("{path}: {why}"), theme.dim());
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

/// The text of the file at `path`: `None` when it does not exist; an error saying why when it
/// is too large or not text.
fn read_small(path: &Path) -> Result<Option<String>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read it for a diff ({e})")),
    };
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
