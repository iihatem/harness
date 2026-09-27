//! Text from config rules, the model or a provider, made safe to print to a terminal.

/// `text` with every control character, and every character that reorders text on screen
/// (bidirectional marks, embeddings, overrides and isolates), replaced by a visible escape such as
/// `\u{1b}`, so it cannot move the cursor, rewrite earlier output or disguise what is printed.
pub fn terminal_safe(text: &str) -> String {
    escape(text, |_| false)
}

/// Like [`terminal_safe`], but keeps `\n` and `\t` as themselves instead of escaping them, so a
/// multi-line answer prints as multiple lines rather than one line full of literal `\n`s. A lone
/// `\r` is still escaped: unlike `\n`/`\t`, it can overwrite everything already printed on the
/// current line.
pub fn terminal_safe_text(text: &str) -> String {
    escape(text, |c| matches!(c, '\n' | '\t'))
}

/// Shared escaping loop: every character is kept as-is when `keep` says so, or when it is neither
/// a control character nor a bidirectional-reordering one; otherwise it is replaced by a visible
/// escape such as `\u{1b}`.
fn escape(text: &str, keep: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if keep(c) {
            out.push(c);
        } else if c.is_control() || is_bidi_control(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_bidi_characters_are_shown_escaped() {
        assert_eq!(
            terminal_safe("run `echo \u{1b}[31mhi\u{7}`\r\n\t"),
            "run `echo \\u{1b}[31mhi\\u{7}`\\r\\n\\t"
        );
        assert_eq!(terminal_safe("\u{9b}2J"), "\\u{9b}2J");
        assert_eq!(
            terminal_safe("a\u{202e}b\u{2066}c\u{200f}"),
            "a\\u{202e}b\\u{2066}c\\u{200f}"
        );
    }

    #[test]
    fn ordinary_text_is_unchanged() {
        let text = "denied by rule `bash:rm -rf *` — it's \"quoted\" \\ é ✓";
        assert_eq!(terminal_safe(text), text);
    }

    #[test]
    fn terminal_safe_text_keeps_newlines_and_tabs() {
        assert_eq!(
            terminal_safe_text("first line\n\tsecond line, tabbed"),
            "first line\n\tsecond line, tabbed"
        );
    }

    #[test]
    fn terminal_safe_text_still_escapes_esc_bel_csi_cr_and_bidi() {
        assert_eq!(
            terminal_safe_text("hi\u{1b}[31m\u{7}\u{9b}2J"),
            "hi\\u{1b}[31m\\u{7}\\u{9b}2J"
        );
        // A lone `\r` (no following `\n`) can overwrite the current line, so it's escaped even
        // though `\n` and `\t` are not.
        assert_eq!(terminal_safe_text("progress\rdone"), "progress\\rdone");
        assert_eq!(
            terminal_safe_text("a\u{202e}b\u{2066}c\u{200f}"),
            "a\\u{202e}b\\u{2066}c\\u{200f}"
        );
    }
}
