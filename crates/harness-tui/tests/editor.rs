//! The input editor, driven by scripted keys and pastes.

use harness_tui::{
    editor::{Edit, Editor},
    style::Theme,
    text::plain,
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::Position,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn ctrl(c: char) -> KeyEvent {
    with(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn type_text(editor: &mut Editor, text: &str) {
    for c in text.chars() {
        assert_eq!(editor.key(key(KeyCode::Char(c))), Edit::Handled);
    }
}

#[test]
fn typing_editing_and_moving_the_cursor() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "fix the bug");
    editor.key(ctrl('w'));
    assert_eq!(editor.text(), "fix the ");
    type_text(&mut editor, "tests");
    editor.key(key(KeyCode::Home));
    editor.key(key(KeyCode::Delete));
    type_text(&mut editor, "F");
    assert_eq!(editor.text(), "Fix the tests");
    editor.key(ctrl('e'));
    editor.key(key(KeyCode::Backspace));
    assert_eq!(editor.text(), "Fix the test");
    editor.key(with(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(editor.cursor(), "Fix the ".len());
    editor.key(ctrl('k'));
    assert_eq!(editor.text(), "Fix the ");
    assert_eq!(editor.key(key(KeyCode::Enter)), Edit::Submit);
    assert_eq!(editor.key(key(KeyCode::Esc)), Edit::Ignored);
}

#[test]
fn alt_enter_shift_enter_ctrl_j_and_a_trailing_backslash_insert_newlines() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "a");
    editor.key(with(KeyCode::Enter, KeyModifiers::ALT));
    type_text(&mut editor, "b");
    editor.key(with(KeyCode::Enter, KeyModifiers::SHIFT));
    type_text(&mut editor, "c");
    editor.key(ctrl('j'));
    type_text(&mut editor, "d\\");
    assert_eq!(editor.key(key(KeyCode::Enter)), Edit::Handled);
    type_text(&mut editor, "e");
    assert_eq!(editor.text(), "a\nb\nc\nd\ne");
    // Up and Down move between rows before they recall history.
    editor.key(key(KeyCode::Up));
    editor.key(key(KeyCode::Up));
    type_text(&mut editor, "C");
    assert_eq!(editor.text(), "a\nb\ncC\nd\ne");
    assert_eq!(
        editor.submit(),
        ("a\nb\ncC\nd\ne".to_string(), "a\nb\ncC\nd\ne".to_string())
    );
    assert!(editor.is_empty());
}

#[test]
fn pasting_a_stack_trace_shows_a_placeholder_and_sends_every_line() {
    let trace: String = (1..=200).map(|i| format!("at frame {i}\r\n")).collect();
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "why does this fail? ");
    editor.paste(&trace);
    type_text(&mut editor, " thanks");
    assert_eq!(
        editor.text(),
        "why does this fail? [Pasted text #1, 200 lines] thanks"
    );
    let (shown, full) = editor.submit();
    assert_eq!(
        shown,
        "why does this fail? [Pasted text #1, 200 lines] thanks"
    );
    assert_eq!(full.lines().filter(|l| l.contains("at frame")).count(), 200);
    assert!(full.starts_with("why does this fail? at frame 1\nat frame 2\n"));
    assert!(!full.contains('\r'));
    // Short pastes are typed in as they are; long single lines collapse too.
    editor.paste("short\npaste");
    assert_eq!(editor.text(), "short\npaste");
    editor.clear();
    editor.paste(&"x".repeat(1_001));
    assert_eq!(editor.text(), "[Pasted text #2, 1 line]");
}

#[test]
fn a_placeholder_is_deleted_whole_and_can_be_expanded_for_editing() {
    let text: String = (1..=12).map(|i| format!("{i}\n")).collect();
    let mut editor = Editor::new(Vec::new());
    editor.paste(&text);
    type_text(&mut editor, "!");
    // The cursor skips over a placeholder as a unit.
    editor.key(key(KeyCode::Left));
    editor.key(key(KeyCode::Left));
    assert_eq!(editor.cursor(), 0);
    editor.key(key(KeyCode::Right));
    assert_eq!(editor.cursor(), "[Pasted text #1, 12 lines]".len());
    assert!(editor.expand_paste());
    assert_eq!(editor.text(), format!("{text}!"));
    assert_eq!(editor.expanded(), format!("{text}!"));
    editor.clear();
    editor.paste(&text);
    editor.key(key(KeyCode::Backspace));
    assert_eq!(editor.text(), "");
    assert_eq!(editor.expanded(), "");
    // Ctrl+O expands too.
    editor.paste(&text);
    assert_eq!(editor.key(ctrl('o')), Edit::Handled);
    assert_eq!(editor.text(), text);
}

#[test]
fn up_and_down_recall_earlier_inputs_and_the_draft() {
    let mut editor = Editor::new(vec!["from the resumed session".to_string()]);
    type_text(&mut editor, "first");
    editor.submit();
    let long: String = (1..=20).map(|i| format!("{i}\n")).collect();
    editor.paste(&long);
    editor.submit();
    type_text(&mut editor, "draft");
    editor.key(key(KeyCode::Up));
    // A long entry comes back collapsed, and is sent in full.
    assert!(
        editor.text().starts_with("[Pasted text #"),
        "{}",
        editor.text()
    );
    assert_eq!(editor.expanded(), long);
    editor.key(key(KeyCode::Up));
    assert_eq!(editor.text(), "first");
    editor.key(key(KeyCode::Up));
    assert_eq!(editor.text(), "from the resumed session");
    assert_eq!(editor.key(key(KeyCode::Up)), Edit::Ignored);
    editor.key(key(KeyCode::Down));
    editor.key(key(KeyCode::Down));
    editor.key(key(KeyCode::Down));
    assert_eq!(editor.text(), "draft");
    assert_eq!(editor.key(key(KeyCode::Down)), Edit::Ignored);
}

#[test]
fn the_editor_wraps_long_lines_and_places_the_cursor() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "abcdefghij");
    editor.key(with(KeyCode::Enter, KeyModifiers::ALT));
    type_text(&mut editor, "xy");
    let (lines, cursor) = editor.render("› ", 8, &Theme::monochrome());
    let text: Vec<String> = lines.iter().map(plain).collect();
    assert_eq!(text, ["› abcdef", "  ghij", "  xy"]);
    assert_eq!(cursor, Position::new(4, 2));
    editor.key(key(KeyCode::Home));
    let (_, cursor) = editor.render("› ", 8, &Theme::monochrome());
    assert_eq!(cursor, Position::new(2, 2));
    // A cursor at the end of a full row goes to the next one.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "abcdef");
    let (lines, cursor) = editor.render("› ", 8, &Theme::monochrome());
    assert_eq!(lines.len(), 2);
    assert_eq!(cursor, Position::new(2, 1));
    // Control characters typed or pasted are shown escaped.
    let mut editor = Editor::new(Vec::new());
    editor.paste("a\u{1b}[2Jb");
    let (lines, _) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(plain(&lines[0]), "› a\\u{1b}[2Jb");
}

// Review Focus: wide characters (CJK, emoji) take two columns, so the cursor and wrapping must
// count columns, not characters.
#[test]
fn wide_characters_take_two_columns_for_the_cursor_and_wrapping() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "日本語 ok");
    let (lines, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(plain(&lines[0]), "› 日本語 ok");
    assert_eq!(cursor, Position::new(2 + 6 + 3, 0));
    editor.key(key(KeyCode::Home));
    editor.key(key(KeyCode::Right));
    let (_, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(cursor, Position::new(4, 0));
    // A wide character never straddles the edge: it moves to the next row.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "abc🙂");
    let (lines, cursor) = editor.render("› ", 6, &Theme::monochrome());
    let text: Vec<String> = lines.iter().map(plain).collect();
    assert_eq!(text, ["› abc", "  🙂"]);
    assert_eq!(cursor, Position::new(4, 1));
}
