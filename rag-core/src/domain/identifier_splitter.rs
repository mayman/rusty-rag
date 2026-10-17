//! IdentifierSplitter: turns text into lowercase words and index tokens.
//!
//! Identifiers are split on case changes, letter/digit boundaries, and the
//! separators `_`, `-`, `.`, `/`. Each word keeps a separator-free `joined` form
//! (`parseConfig` and `parse_config` both give `parseconfig`) and its `parts`.
//!
//! The FTS5 tokenizer configuration (token characters, diacritics) lives in the
//! SQLite adapter. This module decides which lowercase strings are tokens, and keeps
//! them stable under that tokenizer: it only emits characters FTS5 treats as token
//! characters (so circled and squared letters, which are symbols to FTS5, are word
//! breaks here), and it already applies the case folds FTS5 would apply (`fts5_fold`).
//! The adapter has a test that round-trips every token the splitter can emit.

/// Version of the token rules. Bump whenever the tokens emitted for a given text
/// change; an index built with another version must be reindexed.
///
/// 2: circled and squared letters (category So) are no longer word characters, and
/// the characters the FTS5 tokenizer folds are folded in the token.
pub const TOKENIZER_VERSION: u32 = 2;

/// Words of this many characters or more emit no tokens. Long hex strings, base64
/// blobs, and hashes would otherwise explode into one token per letter/digit run.
pub const MAX_WORD_CHARS: usize = 64;

/// One identifier-like word of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// Lowercase form with every separator removed (`parseconfig`).
    pub joined: String,
    /// Lowercase parts in order (`parse`, `config`). Always non-empty.
    pub parts: Vec<String>,
}

impl Word {
    /// Parts that become tokens: single ASCII digits are dropped (`8` in `utf8`).
    /// Query construction uses the same filter so that it matches the index.
    pub fn searchable_parts(&self) -> impl Iterator<Item = &str> {
        self.parts
            .iter()
            .map(String::as_str)
            .filter(|part| !is_single_digit(part))
    }
}

/// Splits text into words and tokens. Used for document text and, identically,
/// for query text.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentifierSplitter;

impl IdentifierSplitter {
    /// The words of `text` in document order. Words of `MAX_WORD_CHARS` or more
    /// characters are dropped entirely.
    pub fn split_words(text: &str) -> Vec<Word> {
        let text = drop_escape_sequences(text);
        words(&text)
            .filter(|run| run.chars().count() < MAX_WORD_CHARS)
            .map(|run| {
                let parts: Vec<String> = split_parts(run)
                    .iter()
                    .map(|p| fts5_fold(&p.to_lowercase()))
                    .collect();
                Word {
                    joined: parts.concat(),
                    parts,
                }
            })
            .collect()
    }

    /// Flat index tokens in document order. Each word contributes its `joined`
    /// form, then its searchable parts when it has more than one part.
    pub fn index_tokens(text: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        for word in Self::split_words(text) {
            tokens.push(word.joined.clone());
            if word.parts.len() > 1 {
                tokens.extend(word.searchable_parts().map(str::to_owned));
            }
        }
        tokens
    }
}

/// Backslash is not a separator. In code it starts an escape such as `\n`, so a
/// backslash followed by `n`, `t` or `r` is dropped together with that letter,
/// which keeps `hello\nworld` from becoming `hello` and `nworld`. Any other
/// backslash is a plain word break. Callers should normalise Windows paths to `/`
/// before indexing or searching, because `C:\new\table` reads as escapes.
fn drop_escape_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(' ');
            if matches!(chars.peek(), Some('n' | 't' | 'r')) {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// True for characters that can appear inside an identifier or path.
fn is_word_char(c: char) -> bool {
    (c.is_alphanumeric() && !is_letter_symbol(c)) || is_separator(c)
}

/// Circled and squared Latin letters: Alphabetic in Unicode, but of category So
/// (symbol), which the FTS5 tokenizer does not keep in a token.
fn is_letter_symbol(c: char) -> bool {
    matches!(c,
        '\u{24b6}'..='\u{24e9}'
        | '\u{1f130}'..='\u{1f149}'
        | '\u{1f150}'..='\u{1f169}'
        | '\u{1f170}'..='\u{1f189}')
}

/// The case folds the FTS5 `unicode61` tokenizer applies on top of lowercasing, for
/// the characters whose lowercase form is not already their folded form. Applying
/// them here keeps every emitted token identical to what the index stores.
fn fts5_fold(lowercase: &str) -> String {
    lowercase
        .chars()
        .map(|c| match c {
            '\u{b5}' => '\u{3bc}',
            '\u{17f}' => 's',
            '\u{345}' | '\u{1fbe}' => '\u{3b9}',
            '\u{3c2}' => '\u{3c3}',
            '\u{3d0}' => '\u{3b2}',
            '\u{3d1}' => '\u{3b8}',
            '\u{3d5}' => '\u{3c6}',
            '\u{3d6}' => '\u{3c0}',
            '\u{3f0}' => '\u{3ba}',
            '\u{3f1}' => '\u{3c1}',
            '\u{3f5}' => '\u{3b5}',
            '\u{1e9b}' => '\u{1e61}',
            other => other,
        })
        .collect()
}

/// Separators join identifier parts; they are trimmed from word edges.
fn is_separator(c: char) -> bool {
    matches!(c, '_' | '-' | '.' | '/')
}

/// Maximal runs of word characters, with separators trimmed from both ends.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_word_char(c))
        .map(|run| run.trim_matches(is_separator))
        .filter(|word| !word.is_empty())
}

/// Splits one word into alphanumeric parts at separators, case changes
/// (`getUser`, `HTTPServer`), and letter/digit boundaries (`utf8`).
fn split_parts(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(&prev) = i.checked_sub(1).and_then(|p| chars.get(p)) {
            let next = chars.get(i + 1);
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_numeric() != c.is_numeric())
                || (prev.is_uppercase()
                    && c.is_uppercase()
                    && next.is_some_and(|n| n.is_lowercase()));
            if boundary && !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn is_single_digit(part: &str) -> bool {
    part.len() == 1 && part.as_bytes()[0].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::{IdentifierSplitter, MAX_WORD_CHARS};

    fn tokens(text: &str) -> Vec<String> {
        IdentifierSplitter::index_tokens(text)
    }

    #[test]
    fn camel_case_keeps_the_joined_identifier_and_its_parts() {
        assert_eq!(
            tokens("getUserName"),
            ["getusername", "get", "user", "name"]
        );
    }

    #[test]
    fn pascal_case_splits_an_acronym_before_the_next_word() {
        assert_eq!(tokens("HTTPServer"), ["httpserver", "http", "server"]);
    }

    #[test]
    fn snake_case_joined_form_has_no_separator() {
        assert_eq!(tokens("parse_config"), ["parseconfig", "parse", "config"]);
    }

    #[test]
    fn camel_and_snake_case_share_the_joined_token() {
        assert_eq!(tokens("parseConfig"), tokens("parse_config"));
    }

    #[test]
    fn kebab_case_splits_on_hyphens() {
        assert_eq!(
            tokens("kebab-case-name"),
            ["kebabcasename", "kebab", "case", "name"]
        );
    }

    #[test]
    fn letter_digit_boundaries_split_tokens_and_drop_single_digit_parts() {
        assert_eq!(tokens("utf8"), ["utf8", "utf"]);
        assert_eq!(tokens("version2Beta"), ["version2beta", "version", "beta"]);
    }

    #[test]
    fn path_separators_split_and_the_joined_path_has_no_separators() {
        assert_eq!(
            tokens("src/foo/bar.rs"),
            ["srcfoobarrs", "src", "foo", "bar", "rs"]
        );
    }

    #[test]
    fn rust_paths_split_on_double_colon() {
        assert_eq!(tokens("std::fs::read"), ["std", "fs", "read"]);
    }

    #[test]
    fn single_lowercase_word_is_one_token() {
        assert_eq!(tokens("config"), ["config"]);
    }

    #[test]
    fn empty_and_whitespace_input_has_no_tokens() {
        assert!(tokens("").is_empty());
        assert!(tokens(" \t\n ").is_empty());
    }

    #[test]
    fn leading_and_trailing_separators_are_not_part_of_the_identifier() {
        assert_eq!(tokens("--verbose"), ["verbose"]);
    }

    #[test]
    fn unicode_letters_are_lowercased_and_split_on_case() {
        assert_eq!(tokens("naïveCafé"), ["naïvecafé", "naïve", "café"]);
    }

    #[test]
    fn sentence_punctuation_is_dropped_and_each_identifier_is_split() {
        assert_eq!(
            tokens("fn parseConfig(path: &Path)"),
            ["fn", "parseconfig", "parse", "config", "path", "path"]
        );
    }

    #[test]
    fn colons_and_brackets_separate_words_without_leaving_empty_tokens() {
        let words: Vec<String> = IdentifierSplitter::split_words("a::b(c)")
            .into_iter()
            .map(|w| w.joined)
            .collect();
        assert_eq!(words, ["a", "b", "c"]);
    }

    #[test]
    fn split_words_gives_the_joined_form_and_the_parts() {
        let words = IdentifierSplitter::split_words("parse_config");
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].joined, "parseconfig");
        assert_eq!(words[0].parts, ["parse", "config"]);
    }

    #[test]
    fn backslash_n_is_an_escape_so_hello_and_world_are_separate_words() {
        assert_eq!(tokens(r"hello\nworld"), ["hello", "world"]);
    }

    #[test]
    fn backslash_before_a_non_escape_letter_is_a_plain_break() {
        assert_eq!(tokens(r"a\b"), ["a", "b"]);
    }

    #[test]
    fn circled_and_squared_letters_are_symbols_not_word_characters() {
        // They are Alphabetic but category So, which the FTS5 tokenizer treats as
        // separators; keeping them inside a word would give tokens FTS5 splits.
        assert_eq!(tokens("\u{24d0}bc"), ["bc"]);
        assert_eq!(tokens("a\u{24b7}b"), ["a", "b"]);
        assert_eq!(tokens("a\u{1f130}b\u{1f170}c"), ["a", "b", "c"]);
        assert!(tokens("\u{1f189}\u{24e9}").is_empty());
    }

    #[test]
    fn characters_the_fts5_tokenizer_would_fold_are_folded_in_the_token() {
        for (given, folded) in [
            ("\u{b5}", "\u{3bc}"),
            ("\u{17f}", "s"),
            ("\u{3c2}", "\u{3c3}"),
            ("\u{3d1}", "\u{3b8}"),
            ("\u{1e9b}", "\u{1e61}"),
            ("\u{1fbe}", "\u{3b9}"),
            ("a\u{345}", "a\u{3b9}"),
        ] {
            assert_eq!(tokens(given), [folded], "{given:?}");
        }
        assert_eq!(
            tokens("\u{3a3}\u{3af}\u{3c3}\u{3c5}\u{3c6}\u{3bf}\u{3c2}"),
            ["σίσυφοσ"]
        );
    }

    #[test]
    fn a_word_of_max_length_emits_no_tokens_and_no_per_digit_explosion() {
        let hex = "a1".repeat(MAX_WORD_CHARS / 2);
        assert_eq!(hex.len(), MAX_WORD_CHARS);
        assert!(tokens(&hex).is_empty());
    }

    #[test]
    fn a_word_just_under_the_limit_is_still_tokenized() {
        let word = "a".repeat(MAX_WORD_CHARS - 1);
        assert_eq!(tokens(&word), [word]);
    }
}

#[cfg(test)]
mod properties {
    use super::IdentifierSplitter;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn splitting_never_panics_on_arbitrary_text(text in any::<String>()) {
            let _ = IdentifierSplitter::index_tokens(&text);
        }

        #[test]
        fn tokens_are_non_empty_and_lowercase(text in any::<String>()) {
            for token in IdentifierSplitter::index_tokens(&text) {
                prop_assert!(!token.is_empty());
                prop_assert_eq!(token.to_lowercase(), token.clone());
            }
        }

        #[test]
        fn joined_forms_contain_no_separators(text in "[a-zA-Z0-9_./\\-]{0,40}") {
            for word in IdentifierSplitter::split_words(&text) {
                prop_assert!(!word.joined.contains(['_', '-', '.', '/']));
            }
        }

        #[test]
        fn an_identifier_keeps_its_lowercased_form(ident in "[a-zA-Z][a-zA-Z0-9]{0,20}") {
            prop_assert!(IdentifierSplitter::index_tokens(&ident).contains(&ident.to_lowercase()));
        }

        #[test]
        fn splitting_is_deterministic(text in any::<String>()) {
            prop_assert_eq!(IdentifierSplitter::index_tokens(&text), IdentifierSplitter::index_tokens(&text));
        }
    }
}
