//! `/` and `@` completion.

use harness_tui::{
    complete::{self, Completer, Offer},
    editor::Editor,
    style::Theme,
    text::plain,
};
use ratatui::style::Modifier;

fn commands() -> Vec<(String, String)> {
    [
        ("help", "List commands"),
        ("init", "Draft an AGENTS.md for this project"),
        ("opsx:propose", "Propose a change"),
        ("opsx:apply", "Implement a change"),
        ("plan", "Plan a change"),
    ]
    .into_iter()
    .map(|(n, d)| (n.to_string(), d.to_string()))
    .collect()
}

fn inserts(offer: &Option<Offer>) -> Vec<String> {
    offer
        .as_ref()
        .map(|o| o.items.iter().map(|i| i.insert.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn a_slash_at_the_start_offers_commands_with_their_descriptions() {
    let dir = tempfile::tempdir().unwrap();
    let mut completer = Completer::new(commands(), dir.path());
    let all = completer.offer("/", 1).unwrap();
    assert_eq!(all.items.len(), 5);
    assert_eq!(all.items[0].detail, "List commands");
    assert_eq!(
        inserts(&completer.offer("/op", 3)),
        ["/opsx:propose", "/opsx:apply"]
    );
    // Names that contain what was typed come after those that start with it.
    assert_eq!(
        inserts(&completer.offer("/p", 2)),
        ["/plan", "/help", "/opsx:propose", "/opsx:apply"]
    );
    assert_eq!(completer.offer("/op", 3).unwrap().replace, 0..3);
    // Not after other text, and not once the arguments begin.
    assert!(completer.offer("see /he", 7).is_none());
    assert!(completer.offer("/help me", 8).is_none());
    assert!(completer.offer("/zzz", 4).is_none());
}

#[test]
fn an_at_offers_workspace_files_matched_fuzzily() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    for file in [
        "src/main.rs",
        "src/lib.rs",
        "docs/maintenance.md",
        "target/debug/main.rs",
        ".git/config",
        ".github/workflows/ci.yml",
    ] {
        let path = ws.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }
    std::fs::write(ws.join(".gitignore"), "target/\n").unwrap();
    let mut completer = Completer::new(commands(), ws);
    let text = "look at @mainrs";
    let offer = completer.offer(text, text.len());
    assert_eq!(inserts(&offer)[0], "@src/main.rs");
    assert_eq!(offer.as_ref().unwrap().replace, 8..text.len());
    let everything = inserts(&completer.offer("@", 1));
    assert!(everything.contains(&"@.github/workflows/ci.yml".to_string()));
    assert!(
        !everything
            .iter()
            .any(|p| p.contains("target") || p.contains(".git/"))
    );
    // An @ inside a word, such as an email address, is not a file.
    assert!(completer.offer("mail me@example", 15).is_none());
    // The cursor must be at the end of the word.
    assert!(completer.offer("@mainrs x", 3).is_none());
}

// Review B, Important 2: files the agent writes or creates during the session are the ordinary
// shape of a coding turn, and must show up in `@` completion without restarting harness.
#[test]
fn a_file_written_after_the_first_query_is_offered_once_the_index_is_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("old.rs"), "x").unwrap();
    let mut completer = Completer::new(commands(), ws);
    // Builds and caches the index.
    assert_eq!(inserts(&completer.offer("@old", 4)), ["@old.rs"]);
    // A tool call writes a new file mid-turn; nothing has told the completer about it yet.
    std::fs::write(ws.join("new_file.rs"), "x").unwrap();
    assert!(completer.offer("@new_file", 9).is_none());
    // The app invalidates the index once the turn that ran the tool ends.
    completer.invalidate_files();
    assert_eq!(inserts(&completer.offer("@new_file", 9)), ["@new_file.rs"]);
    // The old file is still offered too: nothing was dropped by rebuilding.
    assert_eq!(inserts(&completer.offer("@old", 4)), ["@old.rs"]);
}

// Review B, Important 2 (fix note): even without a tracked tool call, an index older than five
// seconds rebuilds on its own at the next `@` query, so a file changed some other way (a shell
// command, another program) is not stuck out for the rest of the session.
#[test]
fn an_index_older_than_five_seconds_rebuilds_without_being_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("old.rs"), "x").unwrap();
    let mut completer = Completer::new(commands(), ws);
    assert_eq!(inserts(&completer.offer("@old", 4)), ["@old.rs"]);
    std::fs::write(ws.join("new_file.rs"), "x").unwrap();
    assert!(completer.offer("@new_file", 9).is_none());
    std::thread::sleep(std::time::Duration::from_millis(5_100));
    assert_eq!(inserts(&completer.offer("@new_file", 9)), ["@new_file.rs"]);
}

#[test]
fn the_list_shows_names_and_descriptions_and_highlights_the_selection() {
    let dir = tempfile::tempdir().unwrap();
    let mut completer = Completer::new(commands(), dir.path());
    let offer = completer.offer("/op", 3).unwrap();
    let lines = complete::render(&offer, 1, 40, &Theme::monochrome());
    let text: Vec<String> = lines.iter().map(plain).collect();
    assert_eq!(
        text,
        [
            "  /opsx:propose  Propose a change",
            "  /opsx:apply    Implement a change",
        ]
    );
    assert!(
        lines[1].spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED)
    );
    assert!(
        !lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED)
    );
}

#[test]
fn a_completed_word_is_replaced_and_collapsed_pastes_stay_collapsed() {
    let mut editor = Editor::new(Vec::new());
    let long: String = (1..=11).map(|i| format!("{i}\n")).collect();
    editor.paste(&long);
    editor.insert(" see @mainrs  please");
    let start = editor.text().find('@').unwrap();
    editor.replace_word(start..start + "@mainrs".len(), "@src/main.rs");
    assert_eq!(
        editor.text(),
        "[Pasted text #1, 11 lines] see @src/main.rs please"
    );
    assert_eq!(
        &editor.text()[..editor.cursor()],
        "[Pasted text #1, 11 lines] see @src/main.rs "
    );
    assert_eq!(editor.expanded(), format!("{long} see @src/main.rs please"));
}

// Review A I2: a command typed in full comes first, ahead of longer names that start with it
// (`/mode` is not completed to `/model`).
#[test]
fn a_command_typed_in_full_comes_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut completer = Completer::new(
        vec![
            ("model".into(), "Switch the model".into()),
            ("mode".into(), "Switch the approval mode".into()),
            ("compact".into(), "Compact".into()),
        ],
        dir.path(),
    );
    assert_eq!(inserts(&completer.offer("/mode", 5)), ["/mode", "/model"]);
    assert_eq!(inserts(&completer.offer("/mod", 4)), ["/model", "/mode"]);
    assert_eq!(inserts(&completer.offer("/model", 6)), ["/model"]);
}
