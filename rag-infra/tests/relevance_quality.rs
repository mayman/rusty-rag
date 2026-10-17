//! Search quality over a fixed synthetic corpus (`tests/relevance/fixtures`): two
//! repositories of Rust, Python, TypeScript, markdown, a changelog, a README, a test
//! file, plus the noise the index policy must skip (vendored, minified, generated,
//! lockfiles). Every query runs through the real `SqliteStore` and the `Search` use
//! case.
//!
//! The ranked queries are a table (`CASES`). The suite asserts that every critical
//! query ranks its expected file first, that every `Want::Top3` query ranks it in the
//! top three, and that at least 80% of the `Want::Top1` queries rank it first. The
//! table with each query, its expected hit and the rank it got is included in the
//! assertion message, so a failure shows the whole picture.

#[path = "relevance/corpus.rs"]
mod corpus;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use corpus::{Corpus, Skipped, NOW};
use rag_core::application::dto::{SearchRequest, SearchResponse};
use rag_core::application::error::{AppError, ErrorCode};
use rag_core::application::search::Search;
use rag_core::application::testkit::FixedClock;
use rag_core::domain::hit_policy::MAX_HITS_PER_FILE;
use rag_core::domain::index_policy::SkipReason;
use rag_core::domain::indexed_at::IndexedAt;

/// What the suite asserts about one ranked query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    /// The expected hit ranks first (and counts toward the 80% bar).
    Top1,
    /// The expected hit is within the top three, always.
    Top3,
}

/// One ranked query: what the user types, the filters, and the hit that must rank.
#[derive(Debug, Clone, Copy)]
struct Case {
    category: &'static str,
    query: &'static str,
    /// `--repo` value (a source name).
    repo: Option<&'static str>,
    /// `--lang` value.
    language: Option<&'static str>,
    /// `--path` glob.
    path_glob: Option<&'static str>,
    /// (source name, repo-relative path) of the hit that should rank.
    expected: (&'static str, &'static str),
    want: Want,
    /// A query that must rank its expected hit first, whatever else changes.
    critical: bool,
}

const fn plain(
    category: &'static str,
    query: &'static str,
    repo: &'static str,
    path: &'static str,
    want: Want,
    critical: bool,
) -> Case {
    Case {
        category,
        query,
        repo: None,
        language: None,
        path_glob: None,
        expected: (repo, path),
        want,
        critical,
    }
}

const CASES: &[Case] = &[
    plain(
        "rare identifier",
        "QUOKKA_FANOUT_LIMIT",
        "widgets",
        "src/limits/fanout.rs",
        Want::Top1,
        true,
    ),
    plain(
        "camelCase query, snake_case source",
        "refreshAccessToken",
        "widgets",
        "src/auth/token_refresh.py",
        Want::Top1,
        true,
    ),
    plain(
        "snake_case query, camelCase source",
        "session_cookie_name",
        "widgets",
        "src/web/session.ts",
        Want::Top1,
        true,
    ),
    plain(
        "camelCase query, snake_case source",
        "retryWithBackoff",
        "widgets",
        "src/util/retry.rs",
        Want::Top1,
        false,
    ),
    plain(
        "PascalCase type (Python)",
        "InvoiceLineItem",
        "widgets",
        "src/billing/invoice.py",
        Want::Top1,
        false,
    ),
    plain(
        "PascalCase type (Rust)",
        "RetryPolicy",
        "widgets",
        "src/util/retry.rs",
        Want::Top1,
        false,
    ),
    plain(
        "PascalCase type (Rust)",
        "HttpServer",
        "widgets",
        "src/net/http_server.rs",
        Want::Top1,
        false,
    ),
    plain(
        "doc heading phrase",
        "Windows path length",
        "widgets",
        "docs/guide/installation.md",
        Want::Top1,
        false,
    ),
    plain(
        "doc heading phrase",
        "Quick start",
        "widgets",
        "README.md",
        Want::Top1,
        false,
    ),
    plain(
        "config key",
        "cache_directory",
        "widgets",
        "docs/guide/configuration.md",
        Want::Top1,
        false,
    ),
    plain(
        "config key",
        "log_rotation_days",
        "widgets",
        "docs/guide/configuration.md",
        Want::Top3,
        false,
    ),
    plain(
        "error message text",
        "config file is missing the required key",
        "widgets",
        "src/config/loader.rs",
        Want::Top3,
        false,
    ),
    plain(
        "error code heading",
        "config_missing_key",
        "widgets",
        "docs/api/errors.md",
        Want::Top3,
        false,
    ),
    plain(
        "path word",
        "billing",
        "widgets",
        "src/billing/invoice.py",
        Want::Top3,
        false,
    ),
    plain(
        "two-word AND",
        "retry backoff",
        "widgets",
        "src/util/retry.rs",
        Want::Top3,
        false,
    ),
    plain(
        "two-word AND",
        "parse config",
        "widgets",
        "src/config/loader.rs",
        Want::Top3,
        false,
    ),
    plain(
        "noisy corpus (vendored twin is skipped)",
        "retry_with_backoff",
        "widgets",
        "src/util/retry.rs",
        Want::Top1,
        false,
    ),
    plain(
        "OR fallback only",
        "QUOKKA_FANOUT_LIMIT zzzmissing",
        "widgets",
        "src/limits/fanout.rs",
        Want::Top3,
        false,
    ),
    plain(
        "OR fallback only",
        "backoff frobnicatorz",
        "widgets",
        "src/util/retry.rs",
        Want::Top3,
        false,
    ),
    Case {
        repo: Some("tools"),
        ..plain(
            "--repo filter",
            "parse_config",
            "tools",
            "src/config/loader.rs",
            Want::Top1,
            false,
        )
    },
    Case {
        language: Some("python"),
        ..plain(
            "--lang filter",
            "refresh",
            "widgets",
            "src/auth/token_refresh.py",
            Want::Top1,
            false,
        )
    },
    Case {
        language: Some("typescript"),
        ..plain(
            "--lang filter",
            "session",
            "widgets",
            "src/web/session.ts",
            Want::Top3,
            false,
        )
    },
    Case {
        path_glob: Some("docs/**"),
        ..plain(
            "--path filter",
            "configuration",
            "widgets",
            "docs/guide/configuration.md",
            Want::Top1,
            false,
        )
    },
    plain(
        "docs before tests (heading + prose)",
        "lease renewal",
        "widgets",
        "docs/guide/leases.md",
        Want::Top1,
        true,
    ),
    plain(
        "docs before tests (*.spec.ts)",
        "circuit breaker",
        "widgets",
        "docs/api/circuit_breaker.md",
        Want::Top1,
        true,
    ),
    plain(
        "docs before tests (*_test.go)",
        "graceful shutdown",
        "widgets",
        "docs/guide/shutdown.md",
        Want::Top1,
        true,
    ),
    plain(
        "docs before tests (tests/ directory)",
        "pagination cursor",
        "widgets",
        "docs/api/pagination.md",
        Want::Top1,
        true,
    ),
    plain(
        "test file is the only match",
        "LEASE_TEST_SENTINEL",
        "widgets",
        "tests/test_leases.py",
        Want::Top1,
        true,
    ),
    plain(
        "test file is the only match",
        "invoice_total_of_no_items_is_zero",
        "widgets",
        "tests/test_billing.py",
        Want::Top1,
        true,
    ),
];

fn clock() -> FixedClock {
    FixedClock::new(IndexedAt::from_epoch_seconds(NOW))
}

fn search(corpus: &Corpus, request: &SearchRequest) -> Result<SearchResponse, AppError> {
    Search::new(&corpus.store, &corpus.store, &clock()).execute(request)
}

fn search_ok(corpus: &Corpus, query: &str) -> SearchResponse {
    search(corpus, &SearchRequest::new(query)).expect("search succeeds")
}

fn request_for(case: &Case) -> SearchRequest {
    let mut request = SearchRequest::new(case.query);
    if let Some(repo) = case.repo {
        request.repos = vec![repo.to_owned()];
    }
    if let Some(language) = case.language {
        request.languages = vec![language.to_owned()];
    }
    if let Some(glob) = case.path_glob {
        request.path = Some(glob.to_owned());
    }
    request
}

/// The 1-based rank of the first hit in `response` from `repo` at `path`.
fn rank_of(response: &SearchResponse, repo: &str, path: &str) -> Option<usize> {
    let slug = corpus::slug_of(repo);
    response
        .results
        .iter()
        .position(|hit| hit.repo == slug && hit.path == path)
        .map(|index| index + 1)
}

struct Outcome {
    case: Case,
    rank: Option<usize>,
    mode: String,
}

fn run(corpus: &Corpus, case: Case) -> Outcome {
    let response = search(corpus, &request_for(&case)).expect("search succeeds");
    let (repo, path) = case.expected;
    Outcome {
        case,
        rank: rank_of(&response, repo, path),
        mode: response.mode,
    }
}

fn table(outcomes: &[Outcome]) -> String {
    let mut out = format!(
        "{:<40} {:<36} {:<42} {:<6} {:<5} {:<11} {}\n",
        "category", "query", "expected", "want", "rank", "mode", "pass"
    );
    for outcome in outcomes {
        let (repo, path) = outcome.case.expected;
        let rank = outcome
            .rank
            .map_or_else(|| "miss".to_owned(), |rank| rank.to_string());
        let passed = match outcome.case.want {
            Want::Top1 => outcome.rank == Some(1),
            Want::Top3 => outcome.rank.is_some_and(|rank| rank <= 3),
        };
        let filters = [
            outcome.case.repo.map(|r| format!("repo={r}")),
            outcome.case.language.map(|l| format!("lang={l}")),
            outcome.case.path_glob.map(|p| format!("path={p}")),
        ];
        let filters: Vec<String> = filters.into_iter().flatten().collect();
        let query = if filters.is_empty() {
            outcome.case.query.to_owned()
        } else {
            format!("{} [{}]", outcome.case.query, filters.join(" "))
        };
        out.push_str(&format!(
            "{:<40} {:<36} {:<42} {:<6} {:<5} {:<11} {}\n",
            outcome.case.category,
            query,
            format!("{repo}:{path}"),
            match outcome.case.want {
                Want::Top1 => "top1",
                Want::Top3 => "top3",
            },
            rank,
            outcome.mode,
            if passed { "yes" } else { "NO" },
        ));
    }
    out
}

#[test]
fn ranked_queries_meet_the_quality_bar() {
    let corpus = corpus::build();
    let outcomes: Vec<Outcome> = CASES.iter().map(|case| run(&corpus, *case)).collect();
    let report = table(&outcomes);

    let critical_misses: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.case.critical && o.rank != Some(1))
        .map(|o| o.case.query)
        .collect();
    assert!(
        critical_misses.is_empty(),
        "critical queries must rank their expected hit first: {critical_misses:?}\n{report}"
    );

    let top3_misses: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.case.want == Want::Top3 && !o.rank.is_some_and(|rank| rank <= 3))
        .map(|o| o.case.query)
        .collect();
    assert!(
        top3_misses.is_empty(),
        "every expected-top-3 query must place its hit in the top three: {top3_misses:?}\n{report}"
    );

    let top1: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.case.want == Want::Top1)
        .collect();
    let top1_hits = top1.iter().filter(|o| o.rank == Some(1)).count();
    assert!(
        top1_hits * 5 >= top1.len() * 4,
        "top-1 for {top1_hits} of {} expected-top-1 queries; the bar is 80%\n{report}",
        top1.len()
    );
}

#[test]
fn the_fixed_corpus_indexes_the_expected_files_and_skips_the_noise() {
    let corpus = corpus::build();
    assert_eq!(
        corpus.skipped,
        vec![
            Skipped {
                repo: "widgets",
                path: "vendor/fastjson/parser.js",
                reason: SkipReason::Vendored,
            },
            Skipped {
                repo: "widgets",
                path: "dist/app.bundle.min.js",
                reason: SkipReason::Minified,
            },
            Skipped {
                repo: "widgets",
                path: "src/gen/api.pb.go",
                reason: SkipReason::Generated,
            },
            Skipped {
                repo: "widgets",
                path: "Cargo.lock",
                reason: SkipReason::Lockfile,
            },
            Skipped {
                repo: "widgets",
                path: "package-lock.json",
                reason: SkipReason::Lockfile,
            },
        ]
    );
    assert_eq!(corpus::DOCS.len(), 34);
}

#[test]
fn noise_that_the_policy_skips_never_appears_in_results() {
    let corpus = corpus::build();
    let skipped: Vec<&str> = corpus.skipped.iter().map(|s| s.path).collect();
    for query in ["retry_with_backoff", "parse_config backoff", "fastjson"] {
        let response = search_ok(&corpus, query);
        assert!(
            response
                .results
                .iter()
                .all(|hit| !skipped.contains(&hit.path.as_str())),
            "{query:?} surfaced a skipped file: {:?}",
            response.results.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }
    for (query, noise) in [
        ("fastjson", "vendored twin"),
        ("protoimpl", "generated protobuf file"),
        ("bundle", "minified bundle"),
    ] {
        let response = search_ok(&corpus, query);
        assert_eq!(
            response.mode, "none",
            "{query:?} should only match the {noise}"
        );
        assert!(response.results.is_empty(), "{query:?}");
    }
}

#[test]
fn a_query_with_no_match_reports_mode_none_and_a_hint() {
    let corpus = corpus::build();
    let response = search_ok(&corpus, "xyzzyplugh");
    assert_eq!(response.mode, "none");
    assert!(response.results.is_empty());
    assert_eq!(response.total_matches, 0);
    assert!(response.hint.is_some());
}

#[test]
fn an_or_fallback_hit_is_reported_as_or_fallback_with_its_hint() {
    let corpus = corpus::build();
    for (query, expected) in [
        ("QUOKKA_FANOUT_LIMIT zzzmissing", "src/limits/fanout.rs"),
        ("backoff frobnicatorz", "src/util/retry.rs"),
    ] {
        let response = search_ok(&corpus, query);
        assert_eq!(response.mode, "or_fallback", "{query:?}");
        assert!(response.hint.is_some(), "{query:?}");
        assert!(
            rank_of(&response, "widgets", expected).is_some_and(|rank| rank <= 3),
            "{query:?}: {expected} in the top three of {:?}",
            response.results.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_repo_filter_keeps_only_that_source() {
    let corpus = corpus::build();
    let unfiltered = search_ok(&corpus, "parse_config");
    let slugs: Vec<&str> = unfiltered.results.iter().map(|h| h.repo.as_str()).collect();
    assert!(
        slugs.contains(&corpus::WIDGETS_SLUG) && slugs.contains(&corpus::TOOLS_SLUG),
        "both repositories define parse_config: {slugs:?}"
    );
    for repo in ["widgets", "tools"] {
        let mut request = SearchRequest::new("parse_config");
        request.repos = vec![repo.to_owned()];
        let response = search(&corpus, &request).expect("search succeeds");
        assert!(!response.results.is_empty(), "{repo}");
        assert!(
            response
                .results
                .iter()
                .all(|hit| hit.repo == corpus::slug_of(repo)),
            "{repo}: {:?}",
            response.results.iter().map(|h| &h.repo).collect::<Vec<_>>()
        );
    }
}

#[test]
fn a_language_filter_keeps_only_that_language() {
    let corpus = corpus::build();
    let mut request = SearchRequest::new("session");
    request.languages = vec!["typescript".to_owned()];
    let response = search(&corpus, &request).expect("search succeeds");
    assert!(!response.results.is_empty());
    assert!(response
        .results
        .iter()
        .all(|hit| hit.language == "typescript"));
}

#[test]
fn a_path_filter_keeps_only_matching_paths() {
    let corpus = corpus::build();
    let mut request = SearchRequest::new("configuration");
    request.path = Some("docs/**".to_owned());
    let response = search(&corpus, &request).expect("search succeeds");
    assert!(!response.results.is_empty());
    assert!(response
        .results
        .iter()
        .all(|hit| hit.path.starts_with("docs/")));
}

#[test]
fn no_file_contributes_more_than_the_per_file_cap() {
    let corpus = corpus::build();
    let mut request = SearchRequest::new("widget");
    request.k = 50;
    let response = search(&corpus, &request).expect("search succeeds");
    assert!(response.results.len() >= 3, "{}", response.results.len());
    let mut per_file: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for hit in &response.results {
        *per_file.entry((&hit.repo, &hit.path)).or_insert(0) += 1;
    }
    let most = per_file.values().copied().max().unwrap_or(0);
    assert!(
        most <= MAX_HITS_PER_FILE,
        "a file has {most} hits: {per_file:?}"
    );
}

#[test]
fn paging_continues_the_same_ranking_without_gaps_or_repeats() {
    let corpus = corpus::build();
    let page = |offset: usize, k: usize| {
        let mut request = SearchRequest::new("widget");
        request.offset = offset;
        request.k = k;
        search(&corpus, &request).expect("search succeeds")
    };
    let first = page(0, 3);
    let second = page(3, 3);
    let whole = page(0, 6);

    assert_eq!(first.results.len(), 3);
    assert!(first.has_more);
    assert_eq!(first.next_offset, Some(3));
    assert_eq!(second.results.first().map(|h| h.rank), Some(4));
    let paged: Vec<&str> = first
        .results
        .iter()
        .chain(&second.results)
        .map(|h| h.chunk.as_str())
        .collect();
    let all: Vec<&str> = whole.results.iter().map(|h| h.chunk.as_str()).collect();
    assert_eq!(all.len(), 6);
    assert_eq!(paged, all);
}

#[test]
fn a_common_word_only_query_returns_promptly() {
    let corpus = corpus::build();
    let started = Instant::now();
    let response = search_ok(&corpus, "the");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "a one-word common query took {elapsed:?}"
    );
    assert!(response.results.len() <= 5);
}

#[test]
fn the_snippet_points_at_the_matching_text_in_the_file() {
    let corpus = corpus::build();
    for (query, needle, path) in [
        (
            "QUOKKA_FANOUT_LIMIT",
            "QUOKKA_FANOUT_LIMIT",
            "src/limits/fanout.rs",
        ),
        (
            "Windows path length",
            "Windows",
            "docs/guide/installation.md",
        ),
    ] {
        let response = search_ok(&corpus, query);
        let hit = &response.results[0];
        assert_eq!(hit.path, path, "{query:?}");
        assert!(hit.snippet.contains(needle), "{query:?}: {:?}", hit.snippet);

        let file_lines: Vec<&str> = corpus::text_of("widgets", path).lines().collect();
        let [first, last] = hit.snippet_lines;
        assert!(
            1 <= first && first <= last && last <= file_lines.len(),
            "{query:?}: snippet_lines {:?} in a {}-line file",
            hit.snippet_lines,
            file_lines.len()
        );
        assert!(
            hit.chunk_lines[0] <= first && last <= hit.chunk_lines[1],
            "{query:?}: snippet_lines {:?} outside chunk_lines {:?}",
            hit.snippet_lines,
            hit.chunk_lines
        );
        assert_eq!(
            hit.snippet,
            file_lines[first - 1..last].join("\n"),
            "{query:?}: the snippet is the file's text at those lines"
        );
        let matching_line = file_lines
            .iter()
            .position(|line| line.contains(needle))
            .map(|index| index + 1)
            .expect("the needle is in the file");
        assert!(
            first <= matching_line && matching_line <= last,
            "{query:?}: line {matching_line} is outside the snippet {:?}",
            hit.snippet_lines
        );
    }
}

#[test]
fn a_stale_source_reports_reindex_required_while_other_sources_still_search() {
    let corpus = corpus::build();
    let connection =
        rusqlite::Connection::open(corpus::database_in(corpus.dir.path())).expect("open");
    let changed = connection
        .execute(
            "UPDATE sources SET chunker_version = 0 WHERE source_id = ?1",
            [corpus::source_id("widgets").as_str()],
        )
        .expect("age the widgets index");
    assert_eq!(changed, 1);
    drop(connection);

    let error = search(&corpus, &SearchRequest::new("QUOKKA_FANOUT_LIMIT"))
        .expect_err("a stale index is not searched");
    assert_eq!(error.code(), ErrorCode::ReindexRequired);

    let mut tools = SearchRequest::new("parse_config");
    tools.repos = vec!["tools".to_owned()];
    let response = search(&corpus, &tools).expect("the tools index is current");
    assert_eq!(response.results[0].path, "src/config/loader.rs");
}
