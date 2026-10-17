//! The search use case: validate the request, resolve the filters, retrieve one page
//! of hits (`retrieval::retrieve`), and build the schema v1 response with snippets,
//! matched terms, relative scores, and hints.
//!
//! The renderer only prints the response. The one thing it still does is enforce the
//! output character budget (`SearchRequest::max_chars`) on the encoded JSON, because
//! only it encodes; it drops whole results from the end and sets `truncated`.

use crate::application::dto::{
    show_chunk_command, SearchRequest, SearchResponse, SearchResultDto, SCHEMA_VERSION,
};
use crate::application::error::{AppError, Scrubbed, StorageError};
use crate::application::ports::{ChunkSearcher, SourceRepository, TOTAL_MATCHES_CAP};
use crate::application::retrieval::{retrieve, Retrieval};
use crate::application::snippet;
use crate::domain::language::Language;
use crate::domain::path_glob::{InvalidGlob, PathGlob};
use crate::domain::query::{Query, QueryMode};
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::Source;

/// The most hits one page may ask for.
pub const MAX_K: usize = 50;
/// The largest offset honoured: the number of matches a searcher counts.
pub const MAX_OFFSET: usize = TOTAL_MATCHES_CAP;

/// Hint when no chunk matched at all.
pub const NO_MATCH_HINT: &str = "no chunk matched the query; try fewer or different words, or check the --repo, --lang and --path filters";
/// Hint when the page is past the last hit.
pub const PAST_END_HINT: &str = "the offset is past the last hit; use a smaller --offset";
/// Hint when only the OR fallback matched.
pub const PARTIAL_HINT: &str = "no chunk matched every word; these hits match some of the words, so check for typos or drop a word";

/// `rag search`.
pub struct Search<'a> {
    sources: &'a dyn SourceRepository,
    searcher: &'a dyn ChunkSearcher,
}

impl<'a> Search<'a> {
    pub fn new(sources: &'a dyn SourceRepository, searcher: &'a dyn ChunkSearcher) -> Self {
        Self { sources, searcher }
    }

    pub fn execute(&self, request: &SearchRequest) -> Result<SearchResponse, AppError> {
        let query = Query::parse(&request.query).map_err(|_| {
            AppError::Usage(Scrubbed::from(
                "the query has no searchable words; use letters or digits",
            ))
        })?;
        if self.sources.list()?.is_empty() {
            return Err(StorageError::IndexMissing.into());
        }
        let filters = self.filters(request)?;
        let k = request.k.clamp(1, MAX_K);
        let offset = request.offset.min(MAX_OFFSET);
        let retrieval = retrieve(self.searcher, &query, &filters, offset, k)?;
        build_response(&query, &retrieval, offset)
    }

    fn filters(&self, request: &SearchRequest) -> Result<SearchFilters, AppError> {
        let mut sources = Vec::new();
        for wanted in &request.repos {
            let found = find_sources(self.sources, wanted)?;
            if found.is_empty() {
                return Err(unknown_source(wanted));
            }
            sources.extend(found.iter().map(|source| source.id().clone()));
        }
        let languages = request
            .languages
            .iter()
            .map(|name| {
                Language::parse(name).ok_or_else(|| {
                    AppError::Usage(Scrubbed::from(format!("unknown language {name:?}")))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let path = request
            .path
            .as_deref()
            .map(|pattern| {
                PathGlob::parse(pattern).map_err(|reason| {
                    AppError::Usage(Scrubbed::from(format!(
                        "invalid --path glob: {}",
                        match reason {
                            InvalidGlob::Empty => "it is empty",
                            InvalidGlob::TooLong => "it is too long",
                            InvalidGlob::TooComplex => "it has too many wildcards",
                        }
                    )))
                })
            })
            .transpose()?;
        Ok(SearchFilters {
            sources,
            languages,
            path,
        })
    }
}

pub(crate) fn unknown_source(wanted: &str) -> AppError {
    AppError::NotFound(Scrubbed::from(format!(
        "no source named {wanted:?}; run `rag list` to see the registered ones"
    )))
}

/// The sources a user-typed `--repo` value names: the source with that name
/// (case-insensitive), else every source with that slug or full id. Empty when
/// nothing matches.
pub(crate) fn find_sources(
    sources: &dyn SourceRepository,
    wanted: &str,
) -> Result<Vec<Source>, AppError> {
    if let Some(source) = sources.find_by_name(wanted)? {
        return Ok(vec![source]);
    }
    let wanted = wanted.to_lowercase();
    Ok(sources
        .list()?
        .into_iter()
        .filter(|source| {
            source.id().slug().to_lowercase() == wanted
                || source.id().as_str().to_lowercase() == wanted
        })
        .collect())
}

/// The schema v1 response for one page. `rank` is the position in the whole result
/// list (`offset` + position in the page), so ranks continue across pages.
fn build_response(
    query: &Query,
    retrieval: &Retrieval,
    offset: usize,
) -> Result<SearchResponse, AppError> {
    let best = retrieval
        .hits
        .iter()
        .map(|hit| hit.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let results = retrieval
        .hits
        .iter()
        .enumerate()
        .map(|(position, hit)| result(query, retrieval, hit, offset + position + 1, best))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SearchResponse {
        schema_version: SCHEMA_VERSION,
        query: query.text().to_owned(),
        mode: retrieval.mode.as_str().to_owned(),
        total_hits: retrieval.total_matches,
        total_is_exact: retrieval.total_is_exact,
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
    best: f64,
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
        score_rel: relative_score(hit.score, best),
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

/// The score as a share of the best score of the response, to three decimals.
/// Scores that cannot be compared (zero, negative, not finite) are all `1.0`.
fn relative_score(score: f64, best: f64) -> f64 {
    if best > 0.0 && best.is_finite() && score.is_finite() {
        (score / best * 1000.0).round() / 1000.0
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
#[cfg(test)]
pub(crate) mod fixtures {
    use crate::application::ports::{ChunkStore, IndexChanges, IndexedFile, SourceRepository};
    use crate::application::testkit::InMemoryStore;
    use crate::domain::chunker::Chunker;
    use crate::domain::git_location::GitLocation;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::oid::{BlobOid, CommitId};
    use crate::domain::repo_path::RepoPath;
    use crate::domain::source::Source;
    use crate::domain::source_id::SourceId;

    pub const COMMIT: &str = "abcdef0123456789abcdef0123456789abcdef01";
    pub const INDEXED_AT: i64 = 1_700_000_000;

    pub fn blob_of(text: &str) -> BlobOid {
        BlobOid::parse(&blake3::hash(text.as_bytes()).to_hex()[..40]).unwrap()
    }

    /// Registers `url` as `name` and indexes `files` at `COMMIT`.
    pub fn index(store: &InMemoryStore, url: &str, name: &str, files: &[(&str, &str)]) -> SourceId {
        let location = GitLocation::parse(url).unwrap();
        let id = SourceId::from_location(&location).unwrap();
        let mut source = Source::register(id.clone(), name, location);
        store.save(&source).unwrap();
        source.mark_ready(
            CommitId::parse(COMMIT).unwrap(),
            IndexedAt::from_epoch_seconds(INDEXED_AT),
        );
        let upserts = files
            .iter()
            .map(|(path, text)| {
                let path = RepoPath::parse(path).unwrap();
                IndexedFile {
                    chunks: Chunker::chunk(&id, &path, text),
                    blob: blob_of(text),
                    size: text.len() as u64,
                    path,
                }
            })
            .collect();
        let changes = IndexChanges {
            upserts,
            removals: Vec::new(),
        };
        store.commit_index(&source, None, &changes).unwrap();
        id
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{index, COMMIT};
    use super::{Search, MAX_K, MAX_OFFSET, NO_MATCH_HINT, PARTIAL_HINT, PAST_END_HINT};
    use crate::application::dto::{ErrorResponse, SearchRequest, SearchResponse};
    use crate::application::error::{AppError, ErrorCode};
    use crate::application::testkit::{InMemoryStore, StampStoredVersions};
    use crate::domain::index_versions::IndexVersions;

    const GOLDEN_TWO_HITS: &str = include_str!("../../tests/golden/search_two_hits.json.golden");
    const GOLDEN_NO_MATCH: &str = include_str!("../../tests/golden/search_no_match.json.golden");
    const GOLDEN_INDEX_MISSING: &str =
        include_str!("../../tests/golden/search_error_index_missing.json.golden");

    const URL: &str = "https://github.com/o/r";

    const PARSE_RS: &str = "pub fn parse_config(text: &str) -> Config {\n    load(text)\n}\n";
    const GUIDE_MD: &str = "# Guide\n\n## Setup\n\nCall parse config before anything else.\n";

    fn store() -> InMemoryStore {
        let store = InMemoryStore::new();
        index(
            &store,
            URL,
            "Docs",
            &[("src/parse.rs", PARSE_RS), ("docs/guide.md", GUIDE_MD)],
        );
        store
    }

    fn run(store: &InMemoryStore, request: &SearchRequest) -> Result<SearchResponse, AppError> {
        Search::new(store, store).execute(request)
    }

    fn ok(store: &InMemoryStore, request: &SearchRequest) -> SearchResponse {
        run(store, request).expect("search succeeds")
    }

    fn json(response: &SearchResponse) -> String {
        serde_json::to_string(response).unwrap()
    }

    #[test]
    fn an_empty_registry_is_index_missing() {
        let error = run(&InMemoryStore::new(), &SearchRequest::new("parse")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::IndexMissing);
        assert_eq!(error.code().exit_code(), 5);
    }

    #[test]
    fn a_query_without_searchable_words_is_a_usage_error() {
        let error = run(&store(), &SearchRequest::new("!!! ???")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Usage);
        assert!(error.to_string().contains("searchable"), "{error}");
    }

    #[test]
    fn a_hit_maps_onto_every_schema_field() {
        let response = ok(&store(), &SearchRequest::new("parse_config"));
        assert_eq!(response.query, "parse_config");
        assert_eq!(response.mode, "and");
        let hit = response
            .results
            .iter()
            .find(|r| r.path == "src/parse.rs")
            .expect("the rust file matches");
        assert!(hit.rank >= 1);
        assert_eq!(hit.id.len(), 64);
        assert_eq!(hit.repo, "github.com/o/r");
        assert_eq!(hit.commit, &COMMIT[..7]);
        assert_eq!(hit.indexed_at, "2023-11-14T22:13:20Z");
        assert_eq!(hit.lines, [1, 3]);
        assert_eq!(hit.language, "rust");
        assert_eq!(hit.heading, None);
        assert_eq!(hit.matched_terms, ["parse_config".replace('_', "")]);
        assert_eq!(hit.snippet, PARSE_RS.trim_end_matches('\n'));
        assert_eq!(hit.show, format!("rag show --chunk {}", hit.id));
    }

    #[test]
    fn a_markdown_hit_carries_its_heading_path() {
        let response = ok(&store(), &SearchRequest::new("anything"));
        let hit = response
            .results
            .iter()
            .find(|r| r.path == "docs/guide.md")
            .expect("the guide matches");
        assert_eq!(hit.heading.as_deref(), Some("Guide"));
        assert_eq!(hit.language, "markdown");
    }

    #[test]
    fn ranks_continue_across_pages() {
        let store = store();
        let mut request = SearchRequest::new("parse config");
        request.k = 1;
        let first = ok(&store, &request);
        assert_eq!(first.results[0].rank, 1);
        assert!(first.has_more);
        request.offset = 1;
        let second = ok(&store, &request);
        assert_eq!(second.offset, 1);
        assert_eq!(second.results[0].rank, 2);
        assert_ne!(first.results[0].id, second.results[0].id);
    }

    #[test]
    fn score_rel_is_relative_to_the_best_hit_of_the_response() {
        let response = ok(&store(), &SearchRequest::new("parse config"));
        assert!(response.results.len() >= 2);
        let best = response
            .results
            .iter()
            .map(|r| r.score)
            .fold(f64::MIN, f64::max);
        for result in &response.results {
            let expected = (result.score / best * 1000.0).round() / 1000.0;
            assert_eq!(result.score_rel, expected);
        }
        assert!(response.results.iter().any(|r| r.score_rel == 1.0));
    }

    #[test]
    fn k_and_offset_are_clamped() {
        let store = store();
        let mut request = SearchRequest::new("parse config");
        request.k = 0;
        assert_eq!(ok(&store, &request).returned, 1, "k is at least 1");
        request.k = 100_000;
        request.offset = 9_999_999;
        let response = ok(&store, &request);
        assert_eq!(response.offset, MAX_OFFSET);
        assert!(response.returned <= MAX_K);
    }

    #[test]
    fn a_page_past_the_last_hit_gets_the_past_end_hint() {
        let mut request = SearchRequest::new("parse_config");
        request.offset = 10;
        let response = ok(&store(), &request);
        assert!(response.results.is_empty());
        assert_eq!(response.hint.as_deref(), Some(PAST_END_HINT));
        assert!(response.total_hits > 0);
    }

    #[test]
    fn no_match_gets_the_no_match_hint() {
        let response = ok(&store(), &SearchRequest::new("zzzunknownzzz"));
        assert_eq!(response.total_hits, 0);
        assert_eq!(response.hint.as_deref(), Some(NO_MATCH_HINT));
    }

    #[test]
    fn the_or_fallback_gets_the_partial_hint() {
        let response = ok(&store(), &SearchRequest::new("parse_config zzzunknownzzz"));
        assert_eq!(response.mode, "or_fallback");
        assert!(!response.results.is_empty());
        assert_eq!(response.hint.as_deref(), Some(PARTIAL_HINT));
    }

    #[test]
    fn a_strong_and_result_has_no_hint() {
        assert_eq!(ok(&store(), &SearchRequest::new("parse_config")).hint, None);
    }

    #[test]
    fn repos_resolve_by_name_case_insensitively_or_by_slug() {
        let store = store();
        index(
            &store,
            "https://github.com/o/other",
            "other",
            &[("src/other.rs", "fn parse_config() {}")],
        );
        for repo in ["docs", "DOCS", "github.com/o/r"] {
            let mut request = SearchRequest::new("parse_config");
            request.repos = vec![repo.to_owned()];
            let response = ok(&store, &request);
            assert!(!response.results.is_empty(), "{repo}");
            assert!(response.results.iter().all(|r| r.repo == "github.com/o/r"));
        }
        let all = ok(&store, &SearchRequest::new("parse_config"));
        assert!(all.results.iter().any(|r| r.repo == "github.com/o/other"));
    }

    #[test]
    fn an_unknown_repo_is_not_found() {
        let mut request = SearchRequest::new("parse");
        request.repos = vec!["nope".to_owned()];
        let error = run(&store(), &request).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound);
        assert!(error.to_string().contains("nope"));
    }

    #[test]
    fn the_language_filter_limits_hits_and_rejects_unknown_languages() {
        let store = store();
        let mut request = SearchRequest::new("parse config");
        request.languages = vec!["Markdown".to_owned()];
        let response = ok(&store, &request);
        assert!(response.results.iter().all(|r| r.language == "markdown"));
        assert!(!response.results.is_empty());
        request.languages = vec!["klingon".to_owned()];
        let error = run(&store, &request).unwrap_err();
        assert_eq!(error.code(), ErrorCode::Usage);
    }

    #[test]
    fn the_path_filter_limits_hits_and_rejects_bad_globs() {
        let store = store();
        let mut request = SearchRequest::new("parse config");
        request.path = Some("docs/**".to_owned());
        let response = ok(&store, &request);
        assert!(response.results.iter().all(|r| r.path.starts_with("docs/")));
        for bad in ["", &"*a".repeat(20), &"x".repeat(1000)] {
            request.path = Some(bad.to_owned());
            let error = run(&store, &request).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Usage, "{bad:?}");
        }
    }

    #[test]
    fn a_stale_source_the_query_touches_requires_a_reindex() {
        let store = store();
        let id = index(
            &store,
            "https://github.com/o/old",
            "old",
            &[("a.rs", "fn parse() {}")],
        );
        store.stamp_stored_versions(
            &id,
            IndexVersions {
                tokenizer: 0,
                ..IndexVersions::current()
            },
        );
        let error = run(&store, &SearchRequest::new("parse")).unwrap_err();
        assert_eq!(error.code(), ErrorCode::ReindexRequired);
        assert_eq!(error.code().exit_code(), 7);
        let mut request = SearchRequest::new("parse");
        request.repos = vec!["docs".to_owned()];
        assert!(
            run(&store, &request).is_ok(),
            "filtered away from the stale source"
        );
    }

    #[test]
    fn total_hits_counts_chunks_before_the_per_file_cap() {
        let store = InMemoryStore::new();
        let big: String = (0..300).map(|i| format!("fn alpha_{i}() {{}}\n")).collect();
        index(
            &store,
            URL,
            "r",
            &[("a.rs", &big), ("b.rs", "fn alpha() {}")],
        );
        let response = ok(&store, &SearchRequest::new("alpha"));
        assert!(response.total_hits >= response.returned);
        let from_a = response.results.iter().filter(|r| r.path == "a.rs").count();
        assert!(from_a <= 2, "at most two hits per file");
    }

    #[test]
    fn two_hit_response_matches_the_golden_file() {
        let store = InMemoryStore::new();
        index(
            &store,
            URL,
            "Docs",
            &[("src/parse.rs", PARSE_RS), ("docs/guide.md", GUIDE_MD)],
        );
        let response = ok(&store, &SearchRequest::new("parse config"));
        assert_eq!(json(&response), GOLDEN_TWO_HITS.trim_end());
    }

    #[test]
    fn an_empty_search_matches_the_golden_file() {
        let response = ok(&store(), &SearchRequest::new("zzzunknownzzz qqqmissingqqq"));
        assert_eq!(json(&response), GOLDEN_NO_MATCH.trim_end());
    }

    #[test]
    fn the_index_missing_error_matches_the_golden_file() {
        let error = run(&InMemoryStore::new(), &SearchRequest::new("parse")).unwrap_err();
        let json = serde_json::to_string(&ErrorResponse::from_error(&error)).unwrap();
        assert_eq!(json, GOLDEN_INDEX_MISSING.trim_end());
    }
}
