//! The search and list use cases against the real SQLite store, with a prepared
//! index written through `commit_index` and read back through a read-only handle,
//! plus a relevance smoke test over a tiny fixed corpus.

use rag_core::application::dto::{SearchRequest, SearchResponse};
use rag_core::application::error::ErrorCode;
use rag_core::application::list_sources::ListSources;
use rag_core::application::ports::{ChunkStore, IndexChanges, IndexedFile, SourceRepository};
use rag_core::application::search::Search;
use rag_core::application::testkit::FixedClock;
use rag_core::domain::chunker::Chunker;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rag_infra::sqlite::SqliteStore;
use tempfile::TempDir;

const URL: &str = "https://github.com/acme/widgets";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

const CORPUS: &[(&str, &str)] = &[
    (
        "src/config/loader.rs",
        "use std::fs;\n\npub fn parse_config(input: &str) -> Config {\n    let raw = fs::read_to_string(input).unwrap();\n    Config::from_toml(&raw)\n}\n",
    ),
    (
        "src/net/http_server.rs",
        "pub struct HttpServer {\n    port: u16,\n}\n\nimpl HttpServer {\n    pub fn listen_and_serve(&self) {\n        accept_connections(self.port);\n    }\n}\n",
    ),
    (
        "src/util/retry.rs",
        "pub fn retry_with_backoff<F: FnMut() -> bool>(mut attempt: F) {\n    let mut delay = 100;\n    while !attempt() {\n        sleep(delay);\n        delay *= 2;\n    }\n}\n",
    ),
    (
        "src/auth/token_refresh.py",
        "def refreshAccessToken(session):\n    response = session.post('/oauth/token')\n    return response.json()['access_token']\n",
    ),
    (
        "src/limits/zebra_quokka.rs",
        "pub const ZEBRA_QUOKKA_LIMIT: usize = 7;\n",
    ),
    (
        "docs/guide/installation.md",
        "# Installation\n\nDownload the release archive and unpack it.\n\n## Troubleshooting\n\nIf the build fails, run cargo clean and try again.\n",
    ),
    (
        "docs/guide/usage.md",
        "# Usage\n\nRun the search command with a query and read the snippets it prints.\n",
    ),
    (
        "README.md",
        "# Widgets\n\nA small toolkit for building widgets. See the guide for details.\n",
    ),
];

struct Fixture {
    _dir: TempDir,
    store: SqliteStore,
}

/// Writes `CORPUS` through `commit_index`, then reopens the database read-only.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let writer = SqliteStore::open_for_write(dir.path()).expect("open for write");
    let location = GitLocation::parse(URL).unwrap();
    let id = SourceId::from_location(&location).unwrap();
    let mut source = Source::register(id.clone(), "widgets", location);
    writer.save(&source).unwrap();
    source.mark_ready(
        CommitId::parse(COMMIT).unwrap(),
        IndexedAt::from_epoch_seconds(1_700_000_000),
    );
    let upserts = CORPUS
        .iter()
        .enumerate()
        .map(|(n, (path, text))| {
            let path = RepoPath::parse(path).unwrap();
            IndexedFile {
                chunks: Chunker::chunk(&id, &path, text),
                blob: BlobOid::parse(&format!("{:040x}", n + 1)).unwrap(),
                size: text.len() as u64,
                path,
            }
        })
        .collect();
    writer
        .commit_index(
            &source,
            None,
            &IndexChanges {
                upserts,
                removals: Vec::new(),
            },
        )
        .expect("commit the prepared index");
    drop(writer);
    let store = SqliteStore::open_for_read(dir.path()).expect("open for read");
    Fixture { _dir: dir, store }
}

/// Two minutes after the fixture was indexed.
fn clock() -> FixedClock {
    FixedClock::new(IndexedAt::from_epoch_seconds(1_700_000_120))
}

fn search(fixture: &Fixture, request: &SearchRequest) -> SearchResponse {
    Search::new(&fixture.store, &fixture.store, &clock())
        .execute(request)
        .expect("search succeeds")
}

fn top_paths(response: &SearchResponse, n: usize) -> Vec<&str> {
    response
        .results
        .iter()
        .take(n)
        .map(|r| r.path.as_str())
        .collect()
}

#[test]
fn a_search_over_the_real_store_fills_the_schema_fields() {
    let fixture = fixture();
    let response = search(&fixture, &SearchRequest::new("parse_config"));
    let hit = &response.results[0];
    assert_eq!(hit.path, "src/config/loader.rs");
    assert_eq!(hit.repo, "github.com/acme/widgets");
    let source = &response.sources["github.com/acme/widgets"];
    assert_eq!(source.commit, &COMMIT[..7]);
    assert_eq!(source.indexed_at, "2023-11-14T22:13:20Z");
    assert_eq!(source.age_seconds, 120);
    assert_eq!(hit.language, "rust");
    assert_eq!(hit.rank, 1);
    assert_eq!(hit.score_rel, 1.0);
    assert!(hit.score > 0.0, "adapter scores are higher-is-better");
    assert_eq!(hit.chunk.len(), 64);
    assert!(hit.snippet.contains("parse_config"));
    assert_eq!(response.mode, "and");
    assert!(response.hint.is_none());
}

/// (query, a path that must be among the top 3 hits)
const RELEVANCE: &[(&str, &str)] = &[
    ("zebra_quokka_limit", "src/limits/zebra_quokka.rs"),
    ("parseConfig", "src/config/loader.rs"),
    ("parse_config", "src/config/loader.rs"),
    ("refresh_access_token", "src/auth/token_refresh.py"),
    ("troubleshooting", "docs/guide/installation.md"),
    ("http server listen", "src/net/http_server.rs"),
    ("installation", "docs/guide/installation.md"),
    ("retry backoff", "src/util/retry.rs"),
];

#[test]
fn relevance_smoke_expected_files_are_in_the_top_three() {
    let fixture = fixture();
    for (query, expected) in RELEVANCE {
        let response = search(&fixture, &SearchRequest::new(query));
        let top = top_paths(&response, 3);
        assert!(
            top.contains(expected),
            "{query:?}: expected {expected} in the top 3, got {top:?}"
        );
    }
}

#[test]
fn a_rare_identifier_ranks_first() {
    let fixture = fixture();
    let response = search(&fixture, &SearchRequest::new("ZEBRA_QUOKKA_LIMIT"));
    assert_eq!(response.results[0].path, "src/limits/zebra_quokka.rs");
}

#[test]
fn a_doc_heading_hit_carries_its_heading() {
    let fixture = fixture();
    let response = search(&fixture, &SearchRequest::new("troubleshooting"));
    let hit = response
        .results
        .iter()
        .find(|r| r.path == "docs/guide/installation.md")
        .unwrap();
    assert!(hit
        .heading
        .as_deref()
        .unwrap_or("")
        .contains("Troubleshooting"));
    assert_eq!(hit.language, "markdown");
}

#[test]
fn a_query_with_one_unknown_word_falls_back_to_or() {
    let fixture = fixture();
    let response = search(&fixture, &SearchRequest::new("retry zzzmissingzzz"));
    assert_eq!(response.mode, "or_fallback");
    assert_eq!(response.results[0].path, "src/util/retry.rs");
    assert!(response.hint.is_some());
}

#[test]
fn filters_apply_on_the_real_store() {
    let fixture = fixture();
    let mut request = SearchRequest::new("the");
    request.languages = vec!["markdown".to_owned()];
    request.path = Some("docs/**".to_owned());
    request.repos = vec!["WIDGETS".to_owned()];
    let response = search(&fixture, &request);
    assert!(!response.results.is_empty());
    assert!(response
        .results
        .iter()
        .all(|r| r.language == "markdown" && r.path.starts_with("docs/")));
}

#[test]
fn hostile_queries_never_error() {
    let fixture = fixture();
    for query in ["AND OR NOT NEAR \"x\" -y a:b * (", "\"", "(((", "a:b:c"] {
        let result = Search::new(&fixture.store, &fixture.store, &clock())
            .execute(&SearchRequest::new(query));
        match result {
            Ok(_) => {}
            Err(error) => assert_eq!(error.code(), ErrorCode::Usage, "{query:?}: {error}"),
        }
    }
}

#[test]
fn an_empty_database_is_index_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open_for_write(dir.path()).unwrap();
    let error = Search::new(&store, &store, &clock())
        .execute(&SearchRequest::new("anything"))
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::IndexMissing);
}

#[test]
fn a_pathological_glob_is_rejected_before_it_reaches_sql() {
    let fixture = fixture();
    let mut request = SearchRequest::new("parse");
    request.path = Some("*a".repeat(40));
    let error = Search::new(&fixture.store, &fixture.store, &clock())
        .execute(&request)
        .unwrap_err();
    assert_eq!(error.code(), ErrorCode::Usage);
}

#[test]
fn list_reports_the_indexed_source_with_its_chunk_count() {
    let fixture = fixture();
    let clock = FixedClock::new(IndexedAt::from_epoch_seconds(1_700_000_120));
    let response = ListSources::new(&fixture.store, &fixture.store, &clock)
        .execute()
        .unwrap();
    let [summary] = &response.sources[..] else {
        panic!("one source")
    };
    assert_eq!(summary.name, "widgets");
    assert_eq!(summary.status, "ready");
    assert_eq!(summary.commit.as_deref(), Some(COMMIT));
    assert_eq!(summary.age_seconds, Some(120));
    assert_eq!(
        summary.chunks,
        fixture.store.chunk_count(&summary_id()).unwrap()
    );
    assert!(summary.chunks >= CORPUS.len());
}

fn summary_id() -> SourceId {
    SourceId::from_location(&GitLocation::parse(URL).unwrap()).unwrap()
}
