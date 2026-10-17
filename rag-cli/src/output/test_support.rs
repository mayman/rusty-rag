//! Builders for output tests: response DTOs with fixed ids, commits, and times.

use std::collections::BTreeMap;

use rag_core::application::dto::{
    SearchResponse, SearchResultDto, SourceInfoDto, DEFAULT_K, DEFAULT_MAX_CHARS, SCHEMA_VERSION,
    UNTRUSTED_CONTENT,
};

/// The short commit of the fixture source.
pub const COMMIT: &str = "abababa";
/// `2023-11-14T22:13:20Z`.
pub const INDEXED_AT: i64 = 1_700_000_000;
/// The slug of the fixture source every result belongs to.
pub const SLUG: &str = "github.com/o/r";

/// A result in `github.com/o/r`; `n` makes the chunk id and the rank. The chunk spans
/// exactly the snippet. `score_rel` is `score` over the best score of the page, as the
/// search use case computes it.
pub fn result(
    n: u64,
    path: &str,
    first_line: usize,
    snippet: &str,
    score: f64,
    score_rel: f64,
) -> SearchResultDto {
    let last_line = first_line + snippet.lines().count().max(1) - 1;
    SearchResultDto {
        chunk: format!("{n:064x}"),
        rank: usize::try_from(n).unwrap(),
        score,
        score_rel,
        repo: SLUG.to_owned(),
        path: path.to_owned(),
        snippet_lines: [first_line, last_line],
        chunk_lines: [first_line, last_line],
        language: "rust".to_owned(),
        heading: None,
        matched_terms: vec!["parse".to_owned(), "config".to_owned()],
        snippet: snippet.to_owned(),
    }
}

/// The fixture source as a response states it: name `r`, 90 seconds old.
pub fn source_info() -> SourceInfoDto {
    SourceInfoDto {
        name: "r".to_owned(),
        commit: COMMIT.to_owned(),
        indexed_at: "2023-11-14T22:13:20Z".to_owned(),
        age_seconds: 90,
    }
}

/// A complete, exact, untruncated page for `query` holding `results`.
pub fn response(query: &str, results: Vec<SearchResultDto>) -> SearchResponse {
    let sources: BTreeMap<String, SourceInfoDto> = if results.is_empty() {
        BTreeMap::new()
    } else {
        BTreeMap::from([(SLUG.to_owned(), source_info())])
    };
    SearchResponse {
        schema_version: SCHEMA_VERSION,
        content: UNTRUSTED_CONTENT,
        query: query.to_owned(),
        mode: if results.is_empty() { "none" } else { "and" }.to_owned(),
        k: DEFAULT_K,
        max_chars: DEFAULT_MAX_CHARS,
        total_matches: results.len(),
        total_is_exact: true,
        returned: results.len(),
        offset: 0,
        next_offset: None,
        has_more: false,
        truncated: false,
        budget_cut: false,
        sources,
        results,
        notices: Vec::new(),
        hint: None,
    }
}
