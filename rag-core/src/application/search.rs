//! The search use case: validate the request, resolve the filters, retrieve one page
//! of hits (`retrieval::retrieve`), and build the schema v1 response with snippets,
//! matched terms, relative scores, and hints.
//!
//! The renderer only prints the response. The one thing it still does is enforce the
//! output character budget (`SearchRequest::max_chars`) on the encoded JSON, because
//! only it encodes; it drops whole results from the end and sets `truncated`.

use std::collections::BTreeMap;

use crate::application::dto::{
    SearchRequest, SearchResponse, SearchResultDto, SourceInfoDto, SCHEMA_VERSION,
    UNTRUSTED_CONTENT,
};
use crate::application::error::{AppError, Scrubbed, StorageError, UnknownSource};
use crate::application::ports::{ChunkSearcher, Clock, SourceRepository, TOTAL_MATCHES_CAP};
use crate::application::retrieval::{retrieve, Retrieval};
use crate::application::snippet;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::language::Language;
use crate::domain::path_glob::{InvalidGlob, PathGlob};
use crate::domain::query::{Query, QueryMode};
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::{Source, SourceStatus};
use crate::domain::source_id::SourceId;
use crate::domain::untrusted_text::sanitize_untrusted;

/// The most hits one page may ask for.
pub const MAX_K: usize = 50;
/// The largest offset honoured: the number of matches a searcher counts.
pub const MAX_OFFSET: usize = TOTAL_MATCHES_CAP;

/// Hint when no chunk matched at all.
pub const NO_MATCH_HINT: &str = "no chunk matched the query (tried all of the words, then any of them); try different words, or check the --repo, --lang and --path filters";
/// Hint when the page is past the last hit.
pub const PAST_END_HINT: &str = "the offset is past the last hit; use a smaller --offset";
/// Hint when nothing matched and `notices` explains that some sources cannot be
/// searched (or failed their last update), so other words are not the first thing to try.
pub const NOTICES_HINT: &str = "nothing matched, and some sources are not searchable or failed their last update; read `notices` before trying other words";
/// Hint when only the OR fallback matched.
pub const PARTIAL_HINT: &str = "no chunk matched every word; these hits match some of the words, so check for typos or drop a word";

/// `rag search`.
pub struct Search<'a> {
    sources: &'a dyn SourceRepository,
    searcher: &'a dyn ChunkSearcher,
    clock: &'a dyn Clock,
}

impl<'a> Search<'a> {
    pub fn new(
        sources: &'a dyn SourceRepository,
        searcher: &'a dyn ChunkSearcher,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            sources,
            searcher,
            clock,
        }
    }

    pub fn execute(&self, request: &SearchRequest) -> Result<SearchResponse, AppError> {
        let query = Query::parse(&request.query).map_err(|_| {
            AppError::Usage(Scrubbed::from(
                "the query has no searchable words; use letters or digits",
            ))
        })?;
        let registered = self.sources.list()?;
        if registered.is_empty() {
            return Err(StorageError::IndexMissing.into());
        }
        let filters = self.filters(request)?;
        let k = request.k.clamp(1, MAX_K);
        let offset = request.offset.min(MAX_OFFSET);
        let retrieval = retrieve(self.searcher, &query, &filters, offset, k)
            .map_err(|error| name_stale_sources(error, &registered, &filters.sources))?;
        let notices = notices(&registered, &filters.sources);
        let page = Page {
            k,
            offset,
            max_chars: request.max_chars,
            now: self.clock.now(),
        };
        build_response(&query, &retrieval, &registered, &page, notices)
    }

    fn filters(&self, request: &SearchRequest) -> Result<SearchFilters, AppError> {
        let mut sources = Vec::new();
        for wanted in &request.repos {
            let found = find_sources(self.sources, wanted)?;
            if found.is_empty() {
                return Err(unknown_source(self.sources, wanted));
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

/// A `ReindexRequired` from the searcher says that a source the query touches is
/// stale but not which; this names it from the catalog (the sources `wanted`, or all
/// of them when `wanted` is empty). Other errors pass through.
fn name_stale_sources(error: StorageError, registered: &[Source], wanted: &[SourceId]) -> AppError {
    if error != StorageError::ReindexRequired {
        return error.into();
    }
    let mut stale = registered
        .iter()
        .filter(|source| wanted.is_empty() || wanted.contains(source.id()))
        .filter(|source| {
            source
                .indexed()
                .is_some_and(|revision| !revision.versions.is_current())
        });
    match stale.next() {
        Some(first) => AppError::StaleIndex {
            source: Scrubbed::new(first.name()),
            also_stale: stale.count(),
        },
        None => error.into(),
    }
}

/// The error for a name or slug that matches no source, listing the names that would.
pub(crate) fn unknown_source(sources: &dyn SourceRepository, wanted: &str) -> AppError {
    match sources.list() {
        Ok(all) => AppError::UnknownSource(UnknownSource::new(
            wanted,
            all.iter().map(|source| source.name().to_owned()),
        )),
        Err(error) => error.into(),
    }
}

/// The sources a user-typed `--repo` value names: the source with that name
/// (case-insensitive), else every source with exactly that slug or full id, else (only
/// when nothing matched exactly) every source whose slug or id matches ignoring case.
/// Slugs are not case-folded: `github.com/O/R` and `github.com/o/r` may be two
/// sources, and an exact match never also returns the other. Empty when nothing
/// matches.
pub(crate) fn find_sources(
    sources: &dyn SourceRepository,
    wanted: &str,
) -> Result<Vec<Source>, AppError> {
    if let Some(source) = sources.find_by_name(wanted)? {
        return Ok(vec![source]);
    }
    let all = sources.list()?;
    let exact: Vec<Source> = all
        .iter()
        .filter(|source| source.id().slug() == wanted || source.id().as_str() == wanted)
        .cloned()
        .collect();
    if !exact.is_empty() {
        return Ok(exact);
    }
    let folded = wanted.to_lowercase();
    Ok(all
        .into_iter()
        .filter(|source| {
            source.id().slug().to_lowercase() == folded
                || source.id().as_str().to_lowercase() == folded
        })
        .collect())
}

/// The effective paging and budget of a request, and the time it ran.
struct Page {
    k: usize,
    offset: usize,
    max_chars: usize,
    now: IndexedAt,
}

/// The `mode` of a response with no matching chunk at all: neither `and` nor
/// `or_fallback` found anything.
const MODE_NONE: &str = "none";

/// The schema v1 response for one page. `rank` is the position in the whole result
/// list (`offset` + position in the page), so ranks continue across pages.
fn build_response(
    query: &Query,
    retrieval: &Retrieval,
    registered: &[Source],
    page: &Page,
    notices: Vec<String>,
) -> Result<SearchResponse, AppError> {
    let offset = page.offset;
    let best = retrieval
        .hits
        .iter()
        .map(|hit| hit.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let results = retrieval
        .hits
        .iter()
        .enumerate()
        .map(|(position, hit)| result(query, hit, offset + position + 1, best))
        .collect::<Result<Vec<_>, _>>()?;
    let mode = if retrieval.total_matches == 0 {
        MODE_NONE
    } else {
        retrieval.mode.as_str()
    };
    Ok(SearchResponse {
        schema_version: SCHEMA_VERSION,
        content: UNTRUSTED_CONTENT,
        query: query.text().to_owned(),
        mode: mode.to_owned(),
        k: page.k,
        max_chars: page.max_chars,
        total_matches: retrieval.total_matches,
        total_is_exact: retrieval.total_is_exact,
        returned: results.len(),
        offset,
        next_offset: retrieval.has_more.then_some(offset + results.len()),
        has_more: retrieval.has_more,
        truncated: false,
        budget_cut: false,
        sources: sources_of(retrieval, registered, page.now)?,
        hint: hint(retrieval, results.len(), !notices.is_empty()),
        results,
        notices,
    })
}

fn result(
    query: &Query,
    hit: &ScoredChunk,
    rank: usize,
    best: f64,
) -> Result<SearchResultDto, AppError> {
    let chunk = &hit.chunk;
    let snippet = snippet::build(&chunk.text, chunk.lines.start(), query);
    Ok(SearchResultDto {
        chunk: chunk.id.as_str().to_owned(),
        rank,
        score: hit.score,
        score_rel: relative_score(hit.score, best),
        repo: chunk.source.slug().to_owned(),
        path: chunk.path.as_str().to_owned(),
        snippet_lines: [snippet.first_line, snippet.last_line],
        chunk_lines: [chunk.lines.start(), chunk.lines.end()],
        language: chunk.language.name().to_owned(),
        heading: chunk.heading().map(|heading| sanitize_untrusted(&heading)),
        matched_terms: snippet
            .matched_terms
            .iter()
            .map(|term| sanitize_untrusted(term))
            .collect(),
        snippet: sanitize_untrusted(&snippet.text),
    })
}

/// The sources behind `results`, once each, keyed by slug.
fn sources_of(
    retrieval: &Retrieval,
    registered: &[Source],
    now: IndexedAt,
) -> Result<BTreeMap<String, SourceInfoDto>, AppError> {
    let missing = || {
        AppError::Storage(StorageError::Failed(Scrubbed::from(
            "a hit's source has no indexed revision",
        )))
    };
    let mut sources = BTreeMap::new();
    for hit in &retrieval.hits {
        let slug = hit.chunk.source.slug();
        if sources.contains_key(slug) {
            continue;
        }
        let revision = retrieval
            .revisions
            .get(&hit.chunk.source)
            .ok_or_else(missing)?;
        let name = registered
            .iter()
            .find(|source| source.id() == &hit.chunk.source)
            .map_or_else(
                || slug.to_owned(),
                |source| sanitize_untrusted(source.name()),
            );
        sources.insert(
            slug.to_owned(),
            SourceInfoDto {
                name,
                commit: revision.commit.short().to_owned(),
                indexed_at: revision.at.to_rfc3339(),
                age_seconds: revision.at.age_seconds(now),
            },
        );
    }
    Ok(sources)
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

fn hint(retrieval: &Retrieval, returned: usize, has_notices: bool) -> Option<String> {
    if returned == 0 {
        let text = if retrieval.total_matches > 0 {
            PAST_END_HINT
        } else if has_notices {
            NOTICES_HINT
        } else {
            NO_MATCH_HINT
        };
        Some(text.to_owned())
    } else if retrieval.mode == QueryMode::OrFallback {
        Some(PARTIAL_HINT.to_owned())
    } else {
        None
    }
}
/// What the caller should know about the sources behind a search: the registered
/// ones that cannot be searched (no indexed revision yet), and those whose last
/// update failed (the old index is still served). Only the sources the search touched
/// (`wanted`, or all when it is empty) are reported.
fn notices(registered: &[Source], wanted: &[SourceId]) -> Vec<String> {
    registered
        .iter()
        .filter(|source| wanted.is_empty() || wanted.contains(source.id()))
        .filter_map(notice)
        .collect()
}

fn notice(source: &Source) -> Option<String> {
    if source.status() == SourceStatus::Removing {
        return None;
    }
    let name = sanitize_untrusted(source.name());
    let slug = source.id().slug();
    let reason = source
        .last_error()
        .map(|error| format!("{}: {}", error.code, error.message));
    match (source.indexed(), source.status()) {
        (None, status) => Some(format!(
            "source {name:?} is registered but not searchable yet (status: {}{}); ask the user to run `rag update {slug}`",
            status.as_str(),
            reason.map_or_else(String::new, |reason| format!(", last error {reason}")),
        )),
        (Some(revision), SourceStatus::Failed) => Some(format!(
            "source {name:?}: the last update failed ({}); results come from its previous index at commit {}; ask the user to run `rag update {slug}`",
            reason.unwrap_or_else(|| "no error was recorded".to_owned()),
            revision.commit.short(),
        )),
        (Some(_), _) => None,
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
    use crate::application::ports::SourceRepository;
    use crate::application::testkit::{FixedClock, InMemoryStore, StampStoredVersions};
    use crate::domain::git_location::GitLocation;
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::source::Source;
    use crate::domain::source_id::SourceId;

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
        let clock = FixedClock::new(IndexedAt::from_epoch_seconds(
            super::fixtures::INDEXED_AT + 90,
        ));
        Search::new(store, store, &clock).execute(request)
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
        assert_eq!(hit.chunk.len(), 64);
        assert_eq!(hit.repo, "github.com/o/r");
        assert_eq!(hit.snippet_lines, [1, 3]);
        assert_eq!(hit.chunk_lines, [1, 3]);
        assert_eq!(hit.language, "rust");
        assert_eq!(hit.heading, None);
        assert_eq!(hit.matched_terms, ["parse_config".replace('_', "")]);
        assert_eq!(hit.snippet, PARSE_RS.trim_end_matches('\n'));
    }

    #[test]
    fn per_source_data_is_stated_once_in_the_sources_object_keyed_by_slug() {
        let response = ok(&store(), &SearchRequest::new("parse config"));
        let [(slug, info)] = response.sources.iter().collect::<Vec<_>>()[..] else {
            panic!("one source: {:?}", response.sources)
        };
        assert_eq!(slug, "github.com/o/r");
        assert_eq!(info.name, "Docs");
        assert_eq!(info.commit, &COMMIT[..7]);
        assert_eq!(info.indexed_at, "2023-11-14T22:13:20Z");
        assert_eq!(info.age_seconds, 90);
        assert!(response.results.iter().all(|r| r.repo == *slug));
    }

    #[test]
    fn the_effective_k_and_max_chars_are_echoed() {
        let mut request = SearchRequest::new("parse config");
        request.k = 100_000;
        request.max_chars = 1234;
        let response = ok(&store(), &request);
        assert_eq!((response.k, response.max_chars), (MAX_K, 1234));
        request.k = 0;
        assert_eq!(ok(&store(), &request).k, 1, "k is at least 1");
    }

    #[test]
    fn next_offset_continues_after_the_page_and_is_null_on_the_last_one() {
        let store = store();
        let mut request = SearchRequest::new("parse config");
        request.k = 1;
        let first = ok(&store, &request);
        assert!(first.has_more);
        assert_eq!(first.next_offset, Some(1));
        assert!(!first.budget_cut, "the use case never cuts the budget");
        request.offset = 1;
        let last = ok(&store, &request);
        assert!(!last.has_more);
        assert_eq!(last.next_offset, None);
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
        assert_ne!(first.results[0].chunk, second.results[0].chunk);
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
        assert!(response.total_matches > 0);
        assert_eq!(response.mode, "and", "it matched; the page is past the end");
    }

    #[test]
    fn no_match_gets_the_no_match_hint() {
        let response = ok(&store(), &SearchRequest::new("zzzunknownzzz"));
        assert_eq!(response.total_matches, 0);
        assert_eq!(response.hint.as_deref(), Some(NO_MATCH_HINT));
    }

    #[test]
    fn zero_results_report_mode_none_never_or_fallback() {
        for query in ["zzzunknownzzz", "zzzunknownzzz qqqmissingqqq"] {
            let response = ok(&store(), &SearchRequest::new(query));
            assert_eq!(response.returned, 0);
            assert_eq!(response.mode, "none", "{query}");
            assert!(response.sources.is_empty());
        }
    }

    #[test]
    fn the_no_match_hint_says_both_and_and_or_were_tried() {
        assert!(
            NO_MATCH_HINT.contains("all of the words"),
            "{NO_MATCH_HINT}"
        );
        assert!(NO_MATCH_HINT.contains("any of them"), "{NO_MATCH_HINT}");
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
        assert!(error.to_string().contains("rag list"), "{error}");
        assert_eq!(error.valid_names(), Some(vec!["Docs"]));
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
        assert_eq!(error.stale_source(), Some("old"), "{error}");
        assert_eq!(
            error.hint().as_deref(),
            Some("ask the user to run `rag reindex old`")
        );
        let mut request = SearchRequest::new("parse");
        request.repos = vec!["docs".to_owned()];
        assert!(
            run(&store, &request).is_ok(),
            "filtered away from the stale source"
        );
    }

    #[test]
    fn total_matches_counts_chunks_before_the_per_file_cap() {
        let store = InMemoryStore::new();
        let big: String = (0..300).map(|i| format!("fn alpha_{i}() {{}}\n")).collect();
        index(
            &store,
            URL,
            "r",
            &[("a.rs", &big), ("b.rs", "fn alpha() {}")],
        );
        let response = ok(&store, &SearchRequest::new("alpha"));
        assert!(response.total_matches >= response.returned);
        let from_a = response.results.iter().filter(|r| r.path == "a.rs").count();
        assert!(from_a <= 2, "at most two hits per file");
    }

    #[test]
    fn every_search_response_marks_its_content_untrusted() {
        let response = ok(&store(), &SearchRequest::new("parse_config"));
        assert_eq!(response.content, "untrusted");
        assert!(json(&response).contains(r#""content":"untrusted""#));
        let empty = ok(&store(), &SearchRequest::new("zzzunknownzzz"));
        assert_eq!(empty.content, "untrusted");
    }

    #[test]
    fn hostile_text_in_snippets_and_headings_is_neutralised_in_core() {
        let store = InMemoryStore::new();
        index(
            &store,
            URL,
            "Docs",
            &[
                (
                    "src/hostile.rs",
                    "pub fn hostile() {\n    // \u{1b}[2J wipe \u{202e}gnp\u{e0041}\u{e0042} \u{9b}31m\n}\n",
                ),
                (
                    "docs/evil.md",
                    "# Head\u{202e}ing \u{e0041}\n\nhostile body\n",
                ),
            ],
        );
        let response = ok(&store, &SearchRequest::new("hostile"));
        assert_eq!(response.results.len(), 2);
        for result in &response.results {
            for text in std::iter::once(&result.snippet).chain(result.heading.as_ref()) {
                assert!(
                    crate::domain::untrusted_text::is_clean(text),
                    "{text:?} still holds control or invisible characters"
                );
            }
        }
        let code = response
            .results
            .iter()
            .find(|r| r.path == "src/hostile.rs")
            .unwrap();
        assert!(
            code.snippet.contains("?[2J wipe ?gnp??"),
            "{:?}",
            code.snippet
        );
        let doc = response
            .results
            .iter()
            .find(|r| r.path == "docs/evil.md")
            .unwrap();
        assert_eq!(doc.heading.as_deref(), Some("Head?ing ?"));
    }

    fn register_only(store: &InMemoryStore, url: &str, name: &str) -> Source {
        let location = GitLocation::parse(url).unwrap();
        let id = SourceId::from_location(&location).unwrap();
        let source = Source::register(id, name, location);
        store.save(&source).unwrap();
        source
    }

    #[test]
    fn registered_but_unsearchable_sources_get_a_specific_hint_and_notice() {
        let store = InMemoryStore::new();
        register_only(&store, "https://github.com/o/new", "new");

        let response = ok(&store, &SearchRequest::new("parse"));

        assert_eq!(response.returned, 0);
        let hint = response.hint.as_deref().unwrap();
        assert_ne!(hint, NO_MATCH_HINT, "never the try-other-words hint");
        assert!(hint.contains("notices"), "{hint}");
        let [notice] = &response.notices[..] else {
            panic!("one notice: {:?}", response.notices)
        };
        assert!(notice.contains("\"new\""), "{notice}");
        assert!(notice.contains("cloning"), "{notice}");
        assert!(notice.contains("ask the user to run"), "{notice}");
        assert!(notice.contains("`rag update github.com/o/new`"), "{notice}");
        assert!(json(&response).contains(r#""notices":["#));
    }

    #[test]
    fn a_failed_first_index_names_the_last_error_in_the_notice() {
        let store = InMemoryStore::new();
        let mut source = register_only(&store, "https://github.com/o/new", "new");
        source.record_failure(
            "network_or_auth",
            "git: operation timed out",
            crate::domain::indexed_at::IndexedAt::from_epoch_seconds(5),
        );
        store.save(&source).unwrap();

        let response = ok(&store, &SearchRequest::new("parse"));

        let notice = &response.notices[0];
        assert!(notice.contains("failed"), "{notice}");
        assert!(
            notice.contains("network_or_auth: git: operation timed out"),
            "{notice}"
        );
    }

    #[test]
    fn a_searchable_source_beside_an_unsearchable_one_keeps_its_results_and_adds_a_notice() {
        let store = store();
        register_only(&store, "https://github.com/o/new", "new");

        let response = ok(&store, &SearchRequest::new("parse_config"));

        assert!(!response.results.is_empty());
        assert_eq!(response.hint, None, "results are fine; only a notice");
        assert_eq!(response.notices.len(), 1);
    }

    #[test]
    fn a_failed_update_still_serves_the_old_index_and_says_so() {
        let store = store();
        let id =
            crate::domain::source_id::SourceId::from_location(&GitLocation::parse(URL).unwrap())
                .unwrap();
        let mut source = store.get(&id).unwrap().unwrap();
        source.record_failure(
            "network_or_auth",
            "offline",
            crate::domain::indexed_at::IndexedAt::from_epoch_seconds(5),
        );
        store.save(&source).unwrap();

        let response = ok(&store, &SearchRequest::new("parse_config"));

        assert!(!response.results.is_empty(), "the old index is served");
        let notice = &response.notices[0];
        assert!(notice.contains("last update failed"), "{notice}");
        assert!(notice.contains("network_or_auth: offline"), "{notice}");
        assert!(notice.contains(&COMMIT[..7]), "{notice}");
    }

    #[test]
    fn a_repo_filter_only_reports_the_sources_it_names() {
        let store = store();
        register_only(&store, "https://github.com/o/new", "new");
        let mut request = SearchRequest::new("parse_config");
        request.repos = vec!["docs".to_owned()];

        let response = ok(&store, &request);

        assert!(response.notices.is_empty(), "{:?}", response.notices);
    }

    #[test]
    fn healthy_sources_produce_no_notices() {
        assert!(ok(&store(), &SearchRequest::new("parse_config"))
            .notices
            .is_empty());
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
