//! Text from config rules, the model or a provider, made safe to print to a terminal.

/// `text` with every control character, and every character that reorders text on screen
/// (bidirectional marks, embeddings, overrides and isolates), replaced by a visible escape such as
/// `\u{1b}`, so it cannot move the cursor, rewrite earlier output or disguise what is printed.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() || is_bidi_control(c) {
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
}
