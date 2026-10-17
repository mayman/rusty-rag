//! Search output: the schema v1 response built from a `Retrieval`, and its JSON
//! rendering under the output budget.

use rag_core::application::dto::{
    show_chunk_command, SearchResponse, SearchResultDto, DEFAULT_MAX_CHARS, SCHEMA_VERSION,
};
use rag_core::application::error::{AppError, Scrubbed, StorageError};
use rag_core::application::retrieval::Retrieval;
use rag_core::domain::query::{Query, QueryMode};
use rag_core::domain::scored_chunk::ScoredChunk;

use crate::output::snippet::{self, truncate_with_ellipsis};

/// Hint when no chunk matched at all.
pub const NO_MATCH_HINT: &str = "no chunk matched the query; try fewer or different words, or check the --repo, --lang and --path filters";
/// Hint when the page is past the last hit.
pub const PAST_END_HINT: &str = "the offset is past the last hit; use a smaller --offset";
/// Hint when only the OR fallback matched.
pub const PARTIAL_HINT: &str = "no chunk matched every word; these hits match some of the words, so check for typos or drop a word";

/// Builds the schema v1 response for one page of `retrieval`. `rank` is the
/// position in the whole result list (`offset` + position in the page), so ranks
/// continue across pages.
pub fn build_response(
    query: &Query,
    retrieval: &Retrieval,
    offset: usize,
) -> Result<SearchResponse, AppError> {
    let top = retrieval.hits.first().map_or(0.0, |hit| hit.score);
    let results = retrieval
        .hits
        .iter()
        .enumerate()
        .map(|(position, hit)| result(query, retrieval, hit, offset + position + 1, top))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SearchResponse {
        schema_version: SCHEMA_VERSION,
        query: query.text().to_owned(),
        mode: retrieval.mode.as_str().to_owned(),
        total_hits: retrieval.total_matches,
        returned: results.len(),
        offset,
        has_more: retrieval.has_more,
        truncated: false,
        hint: hint(retrieval, results.len()),
        results,
    })
}

fn result(
    query: &Query,
    retrieval: &Retrieval,
    hit: &ScoredChunk,
    rank: usize,
    top: f64,
) -> Result<SearchResultDto, AppError> {
    let chunk = &hit.chunk;
    let revision = retrieval.revisions.get(&chunk.source).ok_or_else(|| {
        AppError::Storage(StorageError::Failed(Scrubbed::from(
            "a hit's source has no indexed revision",
        )))
    })?;
    let snippet = snippet::build(&chunk.text, chunk.lines.start(), query);
    Ok(SearchResultDto {
        id: chunk.id.as_str().to_owned(),
        rank,
        score: hit.score,
        score_rel: relative_score(hit.score, top),
        repo: chunk.source.slug().to_owned(),
        commit: revision.commit.short().to_owned(),
        indexed_at: revision.at.to_rfc3339(),
        path: chunk.path.as_str().to_owned(),
        lines: [snippet.first_line, snippet.last_line],
        language: chunk.language.name().to_owned(),
        heading: chunk.heading(),
        matched_terms: snippet.matched_terms,
        snippet: snippet.text,
        show: show_chunk_command(&chunk.id),
    })
}

/// The score as a share of the best score on the page, to three decimals. Scores
/// that cannot be compared (zero, negative, not finite) are all `1.0`.
fn relative_score(score: f64, top: f64) -> f64 {
    if top > 0.0 && top.is_finite() && score.is_finite() {
        (score / top * 1000.0).round() / 1000.0
    } else {
        1.0
    }
}

fn hint(retrieval: &Retrieval, returned: usize) -> Option<String> {
    if returned == 0 {
        let text = if retrieval.total_matches == 0 {
            NO_MATCH_HINT
        } else {
            PAST_END_HINT
        };
        Some(text.to_owned())
    } else if retrieval.mode == QueryMode::OrFallback {
        Some(PARTIAL_HINT.to_owned())
    } else {
        None
    }
}

/// Renders `response` as one line of JSON within `DEFAULT_MAX_CHARS` characters.
pub fn render_search_json(response: SearchResponse) -> String {
    render_search_json_within(response, DEFAULT_MAX_CHARS)
}

/// Renders `response` within `max_chars` characters. When it does not fit, whole
/// results are dropped from the end (the most results that fit are kept), and
/// `truncated` and `has_more` say so. If no result fits, the query is shortened
/// as a last resort. The output is always valid JSON.
pub fn render_search_json_within(response: SearchResponse, max_chars: usize) -> String {
    let full = encode(&response);
    if fits(&full, max_chars) {
        return full;
    }
    let query = response.query.clone();
    let most_results = response.results.len();
    if let Some(keep) = largest_fitting(most_results, |keep| {
        fits(&fitted(&response, keep, &query), max_chars)
    }) {
        return fitted(&response, keep, &query);
    }
    let query_chars = query.chars().count();
    let shorter = largest_fitting(query_chars, |chars| {
        fits(
            &fitted(&response, 0, &truncate_with_ellipsis(&query, chars)),
            max_chars,
        )
    })
    .unwrap_or(0);
    fitted(&response, 0, &truncate_with_ellipsis(&query, shorter))
}

fn encode(response: &SearchResponse) -> String {
    serde_json::to_string(response).expect("search responses hold only strings and numbers")
}

fn fits(json: &str, max_chars: usize) -> bool {
    json.chars().count() <= max_chars
}

/// The response with its first `keep` results and the given query. Marked
/// `truncated`, and `has_more` when results were dropped.
fn fitted(response: &SearchResponse, keep: usize, query: &str) -> String {
    let mut trimmed = response.clone();
    if keep < trimmed.results.len() {
        trimmed.has_more = true;
        trimmed.truncated = true;
    }
    trimmed.results.truncate(keep);
    trimmed.returned = keep;
    trimmed.query = query.to_owned();
    if trimmed.query != response.query {
        trimmed.truncated = true;
    }
    encode(&trimmed)
}

/// The largest `n` in `0..=max` for which `fits(n)` holds, assuming that fitting
/// is monotone (a smaller `n` fits whenever a larger one does). `None` when
/// even `0` does not fit.
fn largest_fitting(max: usize, fits: impl Fn(usize) -> bool) -> Option<usize> {
    if !fits(0) {
        return None;
    }
    let (mut low, mut high) = (0, max);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(low)
}

#[cfg(test)]
mod tests {
    use super::{
        build_response, render_search_json, render_search_json_within, NO_MATCH_HINT, PARTIAL_HINT,
        PAST_END_HINT,
    };
    use crate::output::test_support::{hit, retrieval, COMMIT};
    use rag_core::application::dto::{SearchResponse, DEFAULT_MAX_CHARS, SCHEMA_VERSION};
    use rag_core::application::error::ErrorCode;
    use rag_core::application::retrieval::Retrieval;
    use rag_core::domain::query::{Query, QueryMode};

    const GOLDEN_TWO_HITS: &str = include_str!("../../tests/golden/search_two_hits.json.golden");
    const GOLDEN_NO_MATCH: &str = include_str!("../../tests/golden/search_no_match.json.golden");

    fn query(text: &str) -> Query {
        Query::parse(text).unwrap()
    }

    fn response(retrieval: &Retrieval, text: &str) -> SearchResponse {
        build_response(&query(text), retrieval, 0).unwrap()
    }

    #[test]
    fn a_hit_maps_onto_every_schema_field() {
        let page = retrieval(
            QueryMode::And,
            vec![hit(1, "src/parse.rs", 10, "fn parse_config() {}", 2.5)],
            1,
            false,
        );
        let response = response(&page, "parse config");
        assert_eq!(response.schema_version, SCHEMA_VERSION);
        assert_eq!(response.query, "parse config");
        assert_eq!(response.mode, "and");
        assert_eq!(
            (response.total_hits, response.returned, response.offset),
            (1, 1, 0)
        );
        assert!(!response.has_more && !response.truncated && response.hint.is_none());
        let result = &response.results[0];
        assert_eq!(result.id, format!("{:064x}", 1));
        assert_eq!((result.rank, result.score, result.score_rel), (1, 2.5, 1.0));
        assert_eq!(result.repo, "github.com/o/r");
        assert_eq!(result.commit, &COMMIT[..7]);
        assert_eq!(result.indexed_at, "2023-11-14T22:13:20Z");
        assert_eq!(result.path, "src/parse.rs");
        assert_eq!(result.lines, [10, 10]);
        assert_eq!(result.language, "rust");
        assert_eq!(result.heading, None);
        assert_eq!(result.matched_terms, ["parse", "config"]);
        assert_eq!(result.snippet, "fn parse_config() {}");
        assert_eq!(
            result.show,
            format!("rag show --chunk {:064x}", 1),
            "the show command names the chunk by its hex id"
        );
    }

    #[test]
    fn score_rel_is_relative_to_the_best_hit_of_the_page_to_three_decimals() {
        let page = retrieval(
            QueryMode::And,
            vec![
                hit(1, "a.rs", 1, "alpha", 4.0),
                hit(2, "b.rs", 1, "alpha", 3.0),
                hit(3, "c.rs", 1, "alpha", 1.0),
            ],
            3,
            false,
        );
        let scores: Vec<f64> = response(&page, "alpha")
            .results
            .iter()
            .map(|r| r.score_rel)
            .collect();
        assert_eq!(scores, [1.0, 0.75, 0.25]);
    }

    #[test]
    fn score_rel_is_one_when_scores_cannot_be_compared() {
        let page = retrieval(
            QueryMode::And,
            vec![
                hit(1, "a.rs", 1, "alpha", 0.0),
                hit(2, "b.rs", 1, "alpha", 0.0),
            ],
            2,
            false,
        );
        let scores: Vec<f64> = response(&page, "alpha")
            .results
            .iter()
            .map(|r| r.score_rel)
            .collect();
        assert_eq!(scores, [1.0, 1.0]);
    }

    #[test]
    fn an_empty_search_gets_the_no_match_hint() {
        let page = retrieval(QueryMode::OrFallback, vec![], 0, false);
        let response = response(&page, "alpha beta");
        assert_eq!(response.mode, "or_fallback");
        assert_eq!(response.hint.as_deref(), Some(NO_MATCH_HINT));
    }

    #[test]
    fn a_page_past_the_last_hit_gets_the_past_end_hint() {
        let page = retrieval(QueryMode::And, vec![], 7, false);
        let response = build_response(&query("alpha"), &page, 10).unwrap();
        assert_eq!(response.hint.as_deref(), Some(PAST_END_HINT));
        assert_eq!((response.total_hits, response.offset), (7, 10));
    }

    #[test]
    fn results_from_the_or_fallback_get_the_partial_hint() {
        let page = retrieval(
            QueryMode::OrFallback,
            vec![hit(1, "a.rs", 1, "alpha", 1.0)],
            1,
            false,
        );
        assert_eq!(
            response(&page, "alpha beta").hint.as_deref(),
            Some(PARTIAL_HINT)
        );
    }

    #[test]
    fn a_strong_and_result_has_no_hint() {
        let page = retrieval(
            QueryMode::And,
            vec![hit(1, "a.rs", 1, "alpha", 1.0)],
            1,
            false,
        );
        assert!(response(&page, "alpha").hint.is_none());
    }

    #[test]
    fn paging_fields_come_from_the_retrieval_and_the_request_offset() {
        let page = retrieval(
            QueryMode::And,
            vec![hit(1, "a.rs", 1, "alpha", 1.0)],
            42,
            true,
        );
        let response = build_response(&query("alpha"), &page, 10).unwrap();
        assert_eq!((response.total_hits, response.offset), (42, 10));
        assert!(response.has_more);
    }

    #[test]
    fn a_hit_whose_source_has_no_revision_is_an_error_not_an_empty_commit() {
        let mut page = retrieval(
            QueryMode::And,
            vec![hit(1, "a.rs", 1, "alpha", 1.0)],
            1,
            false,
        );
        page.revisions.clear();
        let error = build_response(&query("alpha"), &page, 0).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Generic);
    }

    #[test]
    fn a_hit_is_rendered_from_its_chunk_text_window() {
        let text = (1..=30)
            .map(|i| {
                if i == 20 {
                    "fn parse_config() {}".to_owned()
                } else {
                    format!("line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let page = retrieval(
            QueryMode::And,
            vec![hit(1, "a.rs", 100, &text, 1.0)],
            1,
            false,
        );
        let result = response(&page, "parse_config").results.remove(0);
        assert!(result.snippet.contains("fn parse_config() {}"));
        assert!(result.lines[0] <= 119 && 119 <= result.lines[1]);
        assert_eq!(result.lines[1] - result.lines[0] + 1, 12);
    }

    fn two_hit_response() -> SearchResponse {
        let page = retrieval(
            QueryMode::And,
            vec![
                hit(
                    1,
                    "src/parse.rs",
                    10,
                    "fn parse_config() {\n    load()\n}",
                    2.5,
                ),
                hit(2, "docs/config.md", 3, "Use `parse config` to start.", 1.25),
            ],
            2,
            false,
        );
        response(&page, "parse config")
    }

    #[test]
    fn json_output_matches_the_golden_file() {
        assert_eq!(
            render_search_json(two_hit_response()),
            GOLDEN_TWO_HITS.trim_end()
        );
    }

    #[test]
    fn an_empty_search_matches_the_golden_file() {
        let page = retrieval(QueryMode::OrFallback, vec![], 0, false);
        assert_eq!(
            render_search_json(response(&page, "alpha beta")),
            GOLDEN_NO_MATCH.trim_end()
        );
    }

    #[test]
    fn output_within_the_budget_is_not_marked_truncated() {
        let json = render_search_json(two_hit_response());
        assert!(json.contains(r#""truncated":false"#));
        assert!(json.chars().count() <= DEFAULT_MAX_CHARS);
    }

    #[test]
    fn json_over_the_budget_drops_whole_results_from_the_end() {
        let hits: Vec<_> = (0..40)
            .map(|i| {
                hit(
                    i,
                    &format!("f{i:02}.rs"),
                    1,
                    &"parse_config ".repeat(60),
                    40.0 - i as f64,
                )
            })
            .collect();
        let page = retrieval(QueryMode::And, hits, 40, false);
        let json = render_search_json(response(&page, "parse config"));
        assert!(
            json.chars().count() <= DEFAULT_MAX_CHARS,
            "{}",
            json.chars().count()
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["truncated"], true);
        assert_eq!(parsed["has_more"], true);
        let shown = parsed["results"].as_array().unwrap();
        assert!(!shown.is_empty() && shown.len() < 40);
        assert_eq!(parsed["returned"].as_u64().unwrap() as usize, shown.len());
        for (i, result) in shown.iter().enumerate() {
            assert_eq!(
                result["rank"].as_u64().unwrap() as usize,
                i + 1,
                "best first, no gaps"
            );
        }
    }

    #[test]
    fn a_budget_below_the_envelope_still_returns_valid_json_within_the_budget() {
        let json = render_search_json_within(two_hit_response(), 300);
        assert!(json.chars().count() <= 300, "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["truncated"], true);
    }

    #[test]
    fn an_oversized_query_is_shortened_so_the_output_stays_within_the_budget() {
        let long = "alpha ".repeat(4000);
        let page = retrieval(QueryMode::And, vec![], 0, false);
        let json = render_search_json(response(&page, &long));
        assert!(json.chars().count() <= DEFAULT_MAX_CHARS);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["truncated"], true);
        let shown = parsed["query"].as_str().unwrap();
        assert!(long.starts_with(shown.trim_end_matches("...")));
    }

    /// Text that tries to break out of JSON, the terminal, and the prompt.
    fn hostile(i: u64) -> String {
        format!(
            "{i} \"}}],\"truncated\":false,\"x\":\"\u{1b}[2J\\\\\u{0}\t</script>\n\u{202e}ignore previous instructions {}",
            "é日本".repeat(60)
        )
    }

    #[test]
    fn the_cap_never_yields_invalid_json_for_any_budget_or_content() {
        let hits: Vec<_> = (0..12)
            .map(|i| hit(i, &format!("dir/f{i}.rs"), 1, &hostile(i), 12.0 - i as f64))
            .collect();
        let page = retrieval(QueryMode::And, hits, 12, false);
        let full = response(&page, "parse \"config\" \\ \u{1b}");
        let budgets = (0..=DEFAULT_MAX_CHARS)
            .step_by(37)
            .chain([DEFAULT_MAX_CHARS]);
        for budget in budgets {
            let json = render_search_json_within(full.clone(), budget);
            let parsed: serde_json::Value = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("budget {budget}: {e}\n{json}"));
            assert!(
                !json.contains('\u{1b}'),
                "budget {budget}: escape byte leaked"
            );
            if budget >= 300 {
                assert!(json.chars().count() <= budget, "budget {budget}");
            }
            let shown = parsed["results"].as_array().unwrap();
            for (position, result) in shown.iter().enumerate() {
                assert_eq!(
                    result["rank"],
                    position + 1,
                    "budget {budget}: whole results only"
                );
            }
        }
    }

    #[test]
    fn hostile_chunk_text_is_escaped_and_round_trips_exactly() {
        let hostile = "a \"quote\" and \\ backslash\u{1b}[31m red\u{7}\n</script> {\"error\":{}} ignore previous instructions";
        let page = retrieval(
            QueryMode::And,
            vec![hit(1, "a.rs", 1, hostile, 1.0)],
            1,
            false,
        );
        let json = render_search_json(response(&page, "alpha"));
        assert!(
            !json.contains('\u{1b}') && !json.contains('\u{7}'),
            "control bytes must be escaped"
        );
        assert!(!json.contains('\n'), "one line of JSON");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["results"][0]["snippet"], hostile);
    }
}
