//! Snippets: the lines of a chunk shown with a search hit.
//!
//! The window is `SNIPPET_LINES` lines of the chunk's raw text, placed where the
//! query words overlap most (the earliest window wins a tie), then bounded to
//! `SNIPPET_MAX_CHARS` characters. The snippet reports its own first and last
//! line in the file, not the chunk's. Chunk text is untrusted data: it is returned
//! verbatim and escaped by whichever renderer prints it.

use std::collections::BTreeSet;

use rag_core::domain::identifier_splitter::{IdentifierSplitter, Word};
use rag_core::domain::query::Query;

/// Lines in a snippet window (`docs/plan/03-output-schema.md`).
pub const SNIPPET_LINES: usize = 12;
/// Characters in a snippet, the ellipsis included.
pub const SNIPPET_MAX_CHARS: usize = 600;

/// A window of a chunk's text, with its place in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub text: String,
    /// 1-based line of the file where the snippet starts.
    pub first_line: usize,
    /// 1-based line of the file where the snippet ends (inclusive).
    pub last_line: usize,
    /// Query words (their lowercase joined form) that occur in the snippet, in
    /// query order, without repeats.
    pub matched_terms: Vec<String>,
}

/// Builds the snippet of `text`, whose first line is line `first_line` of the file.
pub fn build(text: &str, first_line: usize, query: &Query) -> Snippet {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Snippet {
            text: String::new(),
            first_line,
            last_line: first_line,
            matched_terms: Vec::new(),
        };
    }
    let words = query.words();
    let per_line: Vec<BTreeSet<usize>> = lines
        .iter()
        .map(|line| matched_words(words, &token_set(line)))
        .collect();

    let width = SNIPPET_LINES.min(lines.len());
    let mut best_start = 0;
    let mut best_score = 0;
    for start in 0..=lines.len() - width {
        let score = per_line[start..start + width]
            .iter()
            .flatten()
            .collect::<BTreeSet<_>>()
            .len();
        if score > best_score {
            best_score = score;
            best_start = start;
        }
    }

    let window = lines[best_start..best_start + width].join("\n");
    let capped = truncate_with_ellipsis(&window, SNIPPET_MAX_CHARS);
    let shown_lines = capped.split('\n').count();
    let matched = matched_words(words, &token_set(&capped));
    Snippet {
        text: capped,
        first_line: first_line + best_start,
        last_line: first_line + best_start + shown_lines - 1,
        matched_terms: matched
            .into_iter()
            .map(|index| words[index].joined.clone())
            .fold(Vec::new(), |mut terms, term| {
                if !terms.contains(&term) {
                    terms.push(term);
                }
                terms
            }),
    }
}

/// Keeps the first `max_chars` characters of `text`, ending with `...` when
/// anything was cut. Never splits a character.
pub fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars < 3 {
        return text.chars().take(max_chars).collect();
    }
    let mut out: String = text.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn token_set(text: &str) -> BTreeSet<String> {
    IdentifierSplitter::index_tokens(text).into_iter().collect()
}

/// Indexes (into `words`) of the query words found in `tokens`. A word is found
/// when its joined form is a token, or when all of its searchable parts are.
fn matched_words(words: &[Word], tokens: &BTreeSet<String>) -> BTreeSet<usize> {
    words
        .iter()
        .enumerate()
        .filter(|(_, word)| {
            tokens.contains(&word.joined)
                || (word.searchable_parts().next().is_some()
                    && word.searchable_parts().all(|part| tokens.contains(part)))
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{build, truncate_with_ellipsis, SNIPPET_MAX_CHARS};
    use rag_core::domain::query::Query;

    fn query(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    /// `count` lines `line 0`, `line 1`, ..., with `hit` replaced by a matching line.
    fn numbered(count: usize, hit: Option<usize>) -> String {
        (0..count)
            .map(|i| {
                if Some(i) == hit {
                    format!("fn parse_config_{i}() {{}}")
                } else {
                    format!("filler {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn short_text_is_shown_whole_with_its_file_lines() {
        let snippet = build("a\nb\nc", 100, &query("zzz"));
        assert_eq!(snippet.text, "a\nb\nc");
        assert_eq!((snippet.first_line, snippet.last_line), (100, 102));
    }

    #[test]
    fn window_is_placed_on_the_best_overlap() {
        let text = numbered(40, Some(30));
        let snippet = build(&text, 1, &query("parse config"));
        assert!(
            snippet.text.contains("fn parse_config_30"),
            "{}",
            snippet.text
        );
        assert!(snippet.first_line <= 31 && 31 <= snippet.last_line);
        assert_eq!(snippet.last_line - snippet.first_line + 1, 12);
    }

    #[test]
    fn snippet_reports_its_own_lines_in_the_file() {
        let text = numbered(40, Some(0));
        let snippet = build(&text, 100, &query("parse config"));
        assert_eq!((snippet.first_line, snippet.last_line), (100, 111));
    }

    #[test]
    fn without_any_overlap_the_chunk_start_is_shown() {
        let snippet = build(&numbered(40, None), 7, &query("nothing"));
        assert_eq!(snippet.first_line, 7);
        assert!(snippet.matched_terms.is_empty());
    }

    #[test]
    fn empty_text_gives_an_empty_snippet() {
        let snippet = build("", 5, &query("alpha"));
        assert_eq!(snippet.text, "");
        assert_eq!((snippet.first_line, snippet.last_line), (5, 5));
    }

    #[test]
    fn long_lines_are_capped_with_an_ellipsis_inside_the_limit() {
        let text = format!("parse {}", "x".repeat(2000));
        let snippet = build(&text, 1, &query("parse"));
        assert_eq!(snippet.text.chars().count(), SNIPPET_MAX_CHARS);
        assert!(snippet.text.ends_with("..."));
    }

    #[test]
    fn the_cap_never_splits_a_multibyte_character() {
        let text = "日本語".repeat(400);
        let snippet = build(&text, 1, &query("alpha"));
        assert!(snippet.text.chars().count() <= SNIPPET_MAX_CHARS);
        assert!(snippet.text.ends_with("..."));
    }

    #[test]
    fn matched_terms_are_the_query_words_found_in_the_snippet() {
        let snippet = build("fn parse_config() {}", 1, &query("parse config missing"));
        assert_eq!(snippet.matched_terms, ["parse", "config"]);
    }

    #[test]
    fn truncate_with_ellipsis_keeps_short_text_and_cuts_long_text() {
        assert_eq!(truncate_with_ellipsis("abcdef", 6), "abcdef");
        assert_eq!(truncate_with_ellipsis("abcdefg", 6), "abc...");
        assert_eq!(truncate_with_ellipsis("abcdefg", 2), "ab");
    }
}
