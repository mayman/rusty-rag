//! Builders for output tests: response DTOs with fixed ids, commits, and times.

use rag_core::application::dto::{
    SearchResponse, SearchResultDto, SCHEMA_VERSION, UNTRUSTED_CONTENT,
};

/// The short commit every fixture result carries.
pub const COMMIT: &str = "abababa";
/// `2023-11-14T22:13:20Z`.
pub const INDEXED_AT: i64 = 1_700_000_000;

/// A result in `github.com/o/r`; `n` makes the id and the rank. `score_rel` is
/// `score` over the best score of the page, as the search use case computes it.
pub fn result(
    n: u64,
    path: &str,
    first_line: usize,
    snippet: &str,
    score: f64,
    score_rel: f64,
) -> SearchResultDto {
    let id = format!("{n:064x}");
    let last_line = first_line + snippet.lines().count().max(1) - 1;
    SearchResultDto {
        show: format!("rag show --chunk {id}"),
        id,
        rank: usize::try_from(n).unwrap(),
        score,
        score_rel,
        repo: "github.com/o/r".to_owned(),
        commit: COMMIT.to_owned(),
        indexed_at: "2023-11-14T22:13:20Z".to_owned(),
        path: path.to_owned(),
        lines: [first_line, last_line],
        language: "rust".to_owned(),
        heading: None,
        matched_terms: vec!["parse".to_owned(), "config".to_owned()],
        snippet: snippet.to_owned(),
    }
}

/// A complete, exact, untruncated page for `query` holding `results`.
pub fn response(query: &str, results: Vec<SearchResultDto>) -> SearchResponse {
    SearchResponse {
        schema_version: SCHEMA_VERSION,
        content: UNTRUSTED_CONTENT,
        query: query.to_owned(),
        mode: "and".to_owned(),
        total_hits: results.len(),
        total_is_exact: true,
        returned: results.len(),
        offset: 0,
        has_more: false,
        truncated: false,
        results,
        notices: Vec::new(),
        hint: None,
    }
}
