//! The input editor, driven by scripted keys and pastes.

use harness_tui::{
    editor::{Edit, Editor, HISTORY_MAX, PASTE_MAX_BYTES},
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

// Review B's I1: what looks like one character can be several code points: a ZWJ family, a flag
// (two regional indicators), a letter with a combining mark. Editing moved and deleted one code
// point at a time, leaving half a glyph behind.
#[test]
fn keys_move_and_delete_whole_grapheme_clusters() {
    let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
    for (typed, cluster) in [
        ("hi ", family),
        ("go ", "🇯🇵"),
        ("caf", "e\u{301}"),
        ("warn ", "⚠\u{fe0f}"),
    ] {
        let mut editor = Editor::new(Vec::new());
        type_text(&mut editor, &format!("{typed}{cluster}"));
        editor.key(key(KeyCode::Backspace));
        assert_eq!(editor.text(), typed, "Backspace after {cluster:?}");
        // Left and Right step over the cluster; Delete removes it whole.
        type_text(&mut editor, &format!("{cluster}x"));
        editor.key(key(KeyCode::Left));
        editor.key(key(KeyCode::Left));
        assert_eq!(editor.cursor(), typed.len(), "Left over {cluster:?}");
        editor.key(key(KeyCode::Right));
        assert_eq!(
            editor.cursor(),
            typed.len() + cluster.len(),
            "Right over {cluster:?}"
        );
        editor.key(key(KeyCode::Left));
        editor.key(key(KeyCode::Delete));
        assert_eq!(editor.text(), format!("{typed}x"), "Delete of {cluster:?}");
    }
    // The cursor goes after the cluster's two columns.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, &format!("a{family}"));
    let (lines, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(plain(&lines[0]), format!("› a{family}"));
    assert_eq!(cursor, Position::new(2 + 1 + 2, 0));
    type_text(&mut editor, "⚠\u{fe0f}");
    let (_, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(cursor, Position::new(2 + 1 + 2 + 2, 0));
    // A ZWJ typed between two emoji joins them; the cursor goes after the joined cluster.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "👨👩");
    editor.key(key(KeyCode::Left));
    type_text(&mut editor, "\u{200d}");
    assert_eq!(editor.cursor(), editor.text().len());
    let (_, cursor) = editor.render("› ", 40, &Theme::monochrome());
    assert_eq!(cursor, Position::new(4, 0));
    // Up and Down keep the column on screen, not the count of characters.
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "日本語");
    editor.key(with(KeyCode::Enter, KeyModifiers::ALT));
    type_text(&mut editor, "abcd");
    editor.key(key(KeyCode::Up));
    assert_eq!(editor.cursor(), "日本".len());
}

// Review B's M1: a paste was copied twice and counted twice on the UI's thread, however large. One
// larger than 4 MiB, about a million tokens and past any model's window, is refused, and the
// input is left as it was.
#[test]
fn a_paste_too_large_to_send_is_refused() {
    let mut editor = Editor::new(Vec::new());
    type_text(&mut editor, "see ");
    assert!(!editor.paste(&"x".repeat(PASTE_MAX_BYTES + 1)));
    assert_eq!(editor.text(), "see ");
    assert!(editor.paste(&"y\r\n".repeat(PASTE_MAX_BYTES / 3)));
    assert_eq!(
        editor.text(),
        format!("see [Pasted text #1, {} lines]", PASTE_MAX_BYTES / 3)
    );
    assert_eq!(editor.expanded().len(), 4 + 2 * (PASTE_MAX_BYTES / 3));
}

// Review B's M2: a placeholder wider than the row was placed whole, and ran past the edge.
#[test]
fn a_placeholder_wider_than_the_row_is_cut_short() {
    let mut editor = Editor::new(Vec::new());
    editor.paste(&"line\n".repeat(500));
    let (lines, cursor) = editor.render("› ", 16, &Theme::monochrome());
    assert_eq!(lines.len(), 2);
    assert_eq!(plain(&lines[0]), "› [Pasted text …");
    assert!(lines.iter().all(|l| l.width() <= 16));
    assert_eq!(cursor, Position::new(2, 1));
}

// Review B's M3: every input of a resumed session was kept for Up, however many and however
// large. The latest are kept.
#[test]
fn up_recalls_the_latest_inputs_only() {
    let mut history: Vec<String> = (0..HISTORY_MAX + 500)
        .map(|i| format!("input {i}"))
        .collect();
    history.push("z".repeat(PASTE_MAX_BYTES + 1));
    let mut editor = Editor::new(history);
    editor.key(key(KeyCode::Up));
    assert_eq!(editor.text(), format!("input {}", HISTORY_MAX + 499));
    for _ in 0..HISTORY_MAX + 500 {
        editor.key(key(KeyCode::Up));
    }
    assert_eq!(editor.text(), "input 500");
}

// Review A's re-review of wave 2, nit: inputs remembered as the session goes are bounded in
// bytes too, as those of a resumed session are: the latest are kept.
#[test]
fn inputs_remembered_as_the_session_goes_are_bounded_in_bytes() {
    let mut editor = Editor::new(Vec::new());
    let large = |i: usize| format!("{i:04} {}", "x".repeat(PASTE_MAX_BYTES - 5));
    for i in 0..8 {
        editor.remember(&large(i));
    }
    let mut kept = Vec::new();
    loop {
        editor.key(key(KeyCode::Up));
        let text = editor.expanded();
        if kept.last() == Some(&text) {
            break;
        }
        kept.push(text);
    }
    let bytes: usize = kept.iter().map(String::len).sum();
    assert!(bytes <= 16 * 1024 * 1024, "{bytes} bytes kept");
    assert_eq!(kept.len(), 4);
    assert!(kept[0].starts_with("0007 "));
    assert!(kept[3].starts_with("0004 "));
}
