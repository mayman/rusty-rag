//! Untrusted text: whatever came out of an indexed repository (file text, headings,
//! git's error output) is data, never instructions, and must not be able to write to
//! a terminal or to hide itself from a reader.
//!
//! `sanitize_untrusted` is the one rule for it. It replaces control characters
//! (C0, DEL, and the C1 range) and every invisible character that
//! `repo_path::is_hidden_character` names (format characters, bidi controls,
//! zero-width characters, line and paragraph separators, tag characters) with `?`,
//! and keeps TAB and newline, which carry the layout of code.

use crate::domain::repo_path::is_hidden_character;

/// The character that stands in for a removed one.
pub const REPLACEMENT: char = '?';

/// `text` with control and invisible characters replaced by `?`. TAB and `\n` are
/// kept.
pub fn sanitize_untrusted(text: &str) -> String {
    text.chars().map(sanitize_char).collect()
}

fn sanitize_char(c: char) -> char {
    match c {
        '\t' | '\n' => c,
        c if c.is_control() || is_hidden_character(c) => REPLACEMENT,
        c => c,
    }
}

/// Whether `sanitize_untrusted` would leave `text` as it is.
pub fn is_clean(text: &str) -> bool {
    text.chars().all(|c| sanitize_char(c) == c)
}

#[cfg(test)]
mod tests {
    use super::{is_clean, sanitize_untrusted};

    #[test]
    fn ordinary_text_is_unchanged() {
        let text = "fn caf\u{e9}() {\n\treturn \"\u{65e5}\u{672c}\";\n}";
        assert_eq!(sanitize_untrusted(text), text);
        assert!(is_clean(text));
    }

    #[test]
    fn tab_and_newline_are_kept() {
        assert_eq!(sanitize_untrusted("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn terminal_escapes_and_other_c0_controls_are_replaced() {
        assert_eq!(
            sanitize_untrusted("a\u{1b}[2Jb\u{7}c\u{0}d\u{7f}e\rf"),
            "a?[2Jb?c?d?e?f"
        );
    }

    #[test]
    fn c1_controls_are_replaced() {
        // U+009B is the single-character form of the ANSI escape introducer.
        assert_eq!(sanitize_untrusted("a\u{9b}31mb\u{85}c"), "a?31mb?c");
    }

    #[test]
    fn invisible_characters_are_replaced() {
        for c in [
            '\u{202e}',
            '\u{200b}',
            '\u{feff}',
            '\u{2028}',
            '\u{2029}',
            '\u{00ad}',
            '\u{2066}',
            '\u{e0041}',
            '\u{e007f}',
        ] {
            let dirty = format!("a{c}b");
            assert_eq!(sanitize_untrusted(&dirty), "a?b", "{c:?}");
            assert!(!is_clean(&dirty), "{c:?}");
        }
    }

    #[test]
    fn emoji_with_a_variation_selector_survives() {
        let text = "ok \u{2764}\u{fe0f} \u{1f600}";
        assert_eq!(sanitize_untrusted(text), text);
    }
}
