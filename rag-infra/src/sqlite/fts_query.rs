//! Building the FTS5 `MATCH` expression from a domain `Query`.
//!
//! The expression is `(joined OR (part AND part ...))` per word, and the words are
//! combined with `AND` (mode `And`) or `OR` (mode `OrFallback`). Every term is a
//! quoted FTS5 string with embedded quotes doubled, so user text can never be read
//! as an FTS5 operator, column filter, prefix star, or NEAR group: `AND`, `NOT`,
//! `a:b`, `x*`, `-y`, and `(` are ordinary words (and the splitter has already
//! reduced them to lowercase alphanumeric runs).
//!
//! A word matches when the chunk holds its joined form, or every searchable part of
//! it. Parts may sit in different columns because FTS5 evaluates an expression over
//! the whole row.

use rag_core::domain::identifier_splitter::Word;
use rag_core::domain::query::{Query, QueryMode};

/// The `MATCH` argument for `query` under `mode`.
pub(crate) fn match_expression(query: &Query, mode: QueryMode) -> String {
    let separator = match mode {
        QueryMode::And => " AND ",
        QueryMode::OrFallback => " OR ",
    };
    query
        .words()
        .iter()
        .map(word_expression)
        .collect::<Vec<_>>()
        .join(separator)
}

fn word_expression(word: &Word) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in word.searchable_parts() {
        if !parts.contains(&part) {
            parts.push(part);
        }
    }
    if word.parts.len() < 2 || parts.is_empty() {
        return quote(&word.joined);
    }
    let all_parts = parts.iter().map(|part| quote(part)).collect::<Vec<_>>();
    format!("({} OR ({}))", quote(&word.joined), all_parts.join(" AND "))
}

/// An FTS5 string literal: double quotes around the text, inner quotes doubled.
fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{match_expression, quote};
    use rag_core::domain::query::{Query, QueryMode};

    fn expression(text: &str, mode: QueryMode) -> String {
        match_expression(&Query::parse(text).expect("searchable"), mode)
    }

    #[test]
    fn a_plain_word_is_one_quoted_term() {
        assert_eq!(expression("alpha", QueryMode::And), "\"alpha\"");
    }

    #[test]
    fn a_compound_word_matches_joined_or_every_part() {
        assert_eq!(
            expression("parseConfig", QueryMode::And),
            "(\"parseconfig\" OR (\"parse\" AND \"config\"))"
        );
        assert_eq!(
            expression("parse_config", QueryMode::And),
            expression("parseConfig", QueryMode::And)
        );
    }

    #[test]
    fn words_combine_with_and_or_with_or_by_mode() {
        assert_eq!(
            expression("alpha beta", QueryMode::And),
            "\"alpha\" AND \"beta\""
        );
        assert_eq!(
            expression("alpha beta", QueryMode::OrFallback),
            "\"alpha\" OR \"beta\""
        );
    }

    #[test]
    fn single_digit_parts_are_not_searched_like_the_index() {
        assert_eq!(
            expression("utf8", QueryMode::And),
            "(\"utf8\" OR (\"utf\"))"
        );
        assert_eq!(expression("1.2", QueryMode::And), "\"12\"");
    }

    #[test]
    fn repeated_parts_are_searched_once() {
        assert_eq!(
            expression("a_b_a", QueryMode::And),
            "(\"aba\" OR (\"a\" AND \"b\"))"
        );
    }

    #[test]
    fn reserved_words_and_punctuation_become_quoted_ordinary_terms() {
        assert_eq!(
            expression("AND OR NOT NEAR \"x\" -y a:b * (", QueryMode::And),
            "\"and\" AND \"or\" AND \"not\" AND \"near\" AND \"x\" AND \"y\" AND \"a\" AND \"b\""
        );
    }

    #[test]
    fn quoting_doubles_embedded_quotes() {
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
    }
}
