//! Completion while typing: `/` at the start of the input offers commands with their
//! descriptions, and `@` offers workspace files, matched fuzzily (`@mainrs` finds
//! `src/main.rs`).

use std::{
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::text::{Line, Span};

use crate::{
    style::Theme,
    text::{sanitize, width as text_width},
};

/// Items offered at most.
pub const MAX_ITEMS: usize = 8;
/// Workspace files indexed at most, and how long indexing may take.
const MAX_FILES: usize = 20_000;
const INDEX_TIME: Duration = Duration::from_secs(1);
/// How long a built index is trusted before an `@` query rebuilds it anyway, so files changed
/// outside a tracked tool call (a shell command, another program) still turn up before long.
const STALE_AFTER: Duration = Duration::from_secs(5);

/// One thing completion can insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// What replaces the word being completed.
    pub insert: String,
    /// Shown next to it: a command's description.
    pub detail: String,
}

/// What completion offers for the word at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The bytes of the input the chosen item replaces.
    pub replace: Range<usize>,
    pub items: Vec<Item>,
}

/// Completes commands and workspace files.
pub struct Completer {
    /// Command names (without `/`) and descriptions.
    commands: Vec<(String, String)>,
    workspace: PathBuf,
    /// The workspace's files, relative to it, read the first time `@` is typed, and rebuilt
    /// whenever it is invalidated or has gone stale.
    files: Option<Vec<String>>,
    /// When `files` was last built.
    built_at: Option<Instant>,
    matcher: Matcher,
}

impl Completer {
    pub fn new(commands: Vec<(String, String)>, workspace: &Path) -> Completer {
        Completer {
            commands,
            workspace: workspace.to_path_buf(),
            files: None,
            built_at: None,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
        }
    }

    /// Forgets the built file index, so the next `@` query rebuilds it. Call after a turn that
    /// ran tools: it may have written or created files nothing has offered yet.
    pub fn invalidate_files(&mut self) {
        self.files = None;
    }

    /// The commands and their descriptions, as given.
    pub fn commands(&self) -> &[(String, String)] {
        &self.commands
    }

    /// What to offer for `text` with the cursor at byte `cursor`, if anything.
    pub fn offer(&mut self, text: &str, cursor: usize) -> Option<Offer> {
        let before = &text[..cursor];
        // The word the cursor is at the end of.
        let start = before.rfind(char::is_whitespace).map_or(0, |i| {
            i + before[i..].chars().next().map_or(1, char::len_utf8)
        });
        let word = &before[start..];
        let at_word_end = text[cursor..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
        if !at_word_end {
            return None;
        }
        if let Some(name) = word.strip_prefix('/')
            && text[..start].trim().is_empty()
        {
            let items = self.command_items(name);
            return (!items.is_empty()).then_some(Offer {
                replace: start..cursor,
                items,
            });
        }
        if let Some(query) = word.strip_prefix('@') {
            let items = self.file_items(query);
            return (!items.is_empty()).then_some(Offer {
                replace: start..cursor,
                items,
            });
        }
        None
    }

    fn command_items(&self, typed: &str) -> Vec<Item> {
        let item = |(name, description): &(String, String)| Item {
            insert: format!("/{name}"),
            detail: description.clone(),
        };
        let prefix = self
            .commands
            .iter()
            .filter(|(name, _)| name.starts_with(typed));
        let inside = self
            .commands
            .iter()
            .filter(|(name, _)| !name.starts_with(typed) && name.contains(typed));
        prefix.chain(inside).map(item).take(MAX_ITEMS).collect()
    }

    fn file_items(&mut self, query: &str) -> Vec<Item> {
        let stale = self.built_at.is_none_or(|at| at.elapsed() > STALE_AFTER);
        if self.files.is_none() || stale {
            self.files = Some(index(&self.workspace));
            self.built_at = Some(Instant::now());
        }
        let files = self.files.as_ref().expect("just built, if it was missing");
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
        let mut matched = pattern.match_list(files.iter(), &mut self.matcher);
        matched.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.len().cmp(&b.len())).then(a.cmp(b)));
        matched
            .into_iter()
            .take(MAX_ITEMS)
            .map(|(path, _)| Item {
                insert: format!("@{path}"),
                detail: String::new(),
            })
            .collect()
    }
}

/// The workspace's files that git does not ignore, relative to it, `/`-separated, up to
/// [`MAX_FILES`] and as many as can be found in [`INDEX_TIME`].
fn index(workspace: &Path) -> Vec<String> {
    let started = Instant::now();
    let mut files = Vec::new();
    let walk = ignore::WalkBuilder::new(workspace)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk.flatten() {
        if files.len() >= MAX_FILES || started.elapsed() > INDEX_TIME {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(workspace)
            && let Some(path) = relative.to_str()
        {
            files.push(path.to_string());
        }
    }
    files.sort();
    files
}

/// The offer as lines of a list at most `width` columns wide, the `selected` item highlighted.
pub fn render(offer: &Offer, selected: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let name_width = offer
        .items
        .iter()
        .map(|i| text_width(&i.insert))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    offer
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected {
                theme.selected()
            } else {
                theme.plain()
            };
            let name = sanitize(&item.insert);
            let pad = name_width.saturating_sub(text_width(&name));
            let mut spans = vec![Span::styled(format!("  {name}{}", " ".repeat(pad)), style)];
            if !item.detail.is_empty() {
                let room = width.saturating_sub(name_width + 4);
                let detail: String = sanitize(&item.detail)
                    .replace('\n', " ")
                    .chars()
                    .take(room)
                    .collect();
                spans.push(Span::styled(format!("  {detail}"), theme.dim()));
            }
            Line::from(spans)
        })
        .collect()
}
