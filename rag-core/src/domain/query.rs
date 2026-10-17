//! Query: user search text split into words with the same `IdentifierSplitter` that
//! indexes documents, so `parse_config` and `parseConfig` mean the same thing.
//!
//! Policy: run with `QueryMode::And` (every word must match); when that finds
//! nothing and the query has more than one word, run `QueryMode::OrFallback` (any
//! word may match) and report that mode.

use crate::domain::identifier_splitter::{IdentifierSplitter, Word};

/// How the words of a query combine. Also the `mode` reported in search output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    /// Every word must match.
    And,
    /// Any word may match. Used only after `And` found nothing.
    OrFallback,
}

impl QueryMode {
    /// Stable string used in the JSON output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::And => "and",
            Self::OrFallback => "or_fallback",
        }
    }
}

/// The text had no searchable words (empty, whitespace, or punctuation only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSearchableWords;

/// A validated query: the original text and its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    text: String,
    words: Vec<Word>,
}

impl Query {
    pub fn parse(text: &str) -> Result<Self, NoSearchableWords> {
        let words = IdentifierSplitter::split_words(text);
        if words.is_empty() {
            return Err(NoSearchableWords);
        }
        Ok(Self {
            text: text.to_owned(),
            words,
        })
    }

    /// The text as the user typed it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Always at least one word.
    pub fn words(&self) -> &[Word] {
        &self.words
    }

    /// The mode to try after `mode` found nothing, if any. `And` is followed by
    /// `OrFallback` only for queries of several words.
    pub fn fallback_after(&self, mode: QueryMode) -> Option<QueryMode> {
        (mode == QueryMode::And && self.words.len() > 1).then_some(QueryMode::OrFallback)
    }
}

#[cfg(test)]
mod tests {
    use super::{NoSearchableWords, Query, QueryMode};

    fn query(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    #[test]
    fn camel_and_snake_case_queries_have_the_same_words() {
        assert_eq!(query("parseConfig").words(), query("parse_config").words());
    }

    #[test]
    fn words_carry_joined_and_parts() {
        let query = query("parseConfig load");
        let words = query.words();
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].joined, "parseconfig");
        assert_eq!(words[0].parts, ["parse", "config"]);
        assert_eq!(words[1].joined, "load");
    }

    #[test]
    fn original_text_is_kept() {
        assert_eq!(query("  Foo  bar ").text(), "  Foo  bar ");
    }

    #[test]
    fn empty_whitespace_and_punctuation_only_text_has_no_words() {
        for text in ["", "   ", "!!! ??? ...", "\"\" -- ::"] {
            assert_eq!(Query::parse(text), Err(NoSearchableWords), "{text:?}");
        }
    }

    #[test]
    fn reserved_fts_words_and_punctuation_are_ordinary_words() {
        let words: Vec<String> = query("AND OR NOT NEAR \"x\" -y a:b")
            .words()
            .iter()
            .map(|w| w.joined.clone())
            .collect();
        assert_eq!(words, ["and", "or", "not", "near", "x", "y", "a", "b"]);
    }

    #[test]
    fn mode_strings_are_stable() {
        assert_eq!(QueryMode::And.as_str(), "and");
        assert_eq!(QueryMode::OrFallback.as_str(), "or_fallback");
    }

    #[test]
    fn and_falls_back_to_or_for_multi_word_queries() {
        assert_eq!(
            query("alpha beta").fallback_after(QueryMode::And),
            Some(QueryMode::OrFallback)
        );
    }

    #[test]
    fn single_word_queries_have_no_fallback() {
        assert_eq!(query("alpha").fallback_after(QueryMode::And), None);
    }

    #[test]
    fn or_fallback_has_no_further_fallback() {
        assert_eq!(
            query("alpha beta").fallback_after(QueryMode::OrFallback),
            None
        );
    }
}
