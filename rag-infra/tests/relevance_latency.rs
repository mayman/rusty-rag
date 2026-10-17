//! Latency guard: a larger index (about 22,000 chunks in a real SQLite store) and a
//! common-word AND query run 20 times through the `Search` use case.
//!
//! The bounds are deliberately loose, for the unoptimised test profile on a loaded
//! machine: the median of the 20 runs must stay under 2 s and the p95 under 5 s.
//! They exist to catch a change that makes the query scan every chunk (an order of
//! magnitude or more), not a few percent. The summary (min, median, p95, max) is
//! part of every assertion message. (The query plan is checked structurally in the
//! `sqlite::search` unit tests.)

use std::time::{Duration, Instant};

use rag_core::application::dto::SearchRequest;
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

const FILES: usize = 2_000;
const LINES_PER_FILE: usize = 600;
const MIN_CHUNKS: usize = 20_000;
const RUNS: usize = 20;
const MEDIAN_BOUND: Duration = Duration::from_secs(2);
const P95_BOUND: Duration = Duration::from_secs(5);
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A source file with no blank lines, so the chunker cuts it into full windows.
fn generated_text(file: usize) -> String {
    (0..LINES_PER_FILE)
        .map(|line| format!("let config_{file}_{line} = the_value({line}); // the config line\n"))
        .collect()
}

/// Indexes `FILES` generated files as one source and returns the store, read-only.
fn large_index(directory: &std::path::Path) -> (SqliteStore, SourceId) {
    let writer = SqliteStore::open_for_write(directory).expect("open for write");
    let location = GitLocation::parse("https://github.com/acme/large").expect("url");
    let id = SourceId::from_location(&location).expect("source id");
    let mut source = Source::register(id.clone(), "large", location);
    writer.save(&source).expect("save the source");
    source.mark_ready(
        CommitId::parse(COMMIT).expect("commit"),
        IndexedAt::from_epoch_seconds(1_700_000_000),
    );
    let upserts = (0..FILES)
        .map(|file| {
            let path =
                RepoPath::parse(&format!("src/generated/module_{file:05}.rs")).expect("path");
            let text = generated_text(file);
            IndexedFile {
                chunks: Chunker::chunk(&id, &path, &text),
                blob: BlobOid::parse(&format!("{:040x}", file + 1)).expect("blob"),
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
        .expect("commit the large index");
    drop(writer);
    let reader = SqliteStore::open_for_read(directory).expect("open for read");
    (reader, id)
}

/// Min, median, p95 and max of the timings, as one line for the assertion message.
fn summary(timings: &[Duration]) -> String {
    let mut sorted = timings.to_vec();
    sorted.sort();
    let at = |percent: usize| sorted[((percent * sorted.len()).div_ceil(100)).max(1) - 1];
    format!(
        "{} runs: min {:?}, median {:?}, p95 {:?}, max {:?}",
        sorted.len(),
        sorted.first().copied().unwrap_or_default(),
        at(50),
        at(95),
        sorted.last().copied().unwrap_or_default(),
    )
}

fn percentile(timings: &[Duration], percent: usize) -> Duration {
    let mut sorted = timings.to_vec();
    sorted.sort();
    sorted[((percent * sorted.len()).div_ceil(100)).max(1) - 1]
}

#[test]
fn twenty_common_word_queries_over_twenty_thousand_chunks_meet_the_latency_bounds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, id) = large_index(dir.path());
    let chunks = store.chunk_count(&id).expect("chunk count");
    assert!(chunks >= MIN_CHUNKS, "only {chunks} chunks indexed");

    let clock = FixedClock::new(IndexedAt::from_epoch_seconds(1_700_000_120));
    let search = Search::new(&store, &store, &clock);
    let mut timings = Vec::with_capacity(RUNS);
    let mut last_total = 0;
    for _ in 0..RUNS {
        let started = Instant::now();
        let response = search
            .execute(&SearchRequest::new("the config"))
            .expect("the common-word search succeeds");
        timings.push(started.elapsed());
        last_total = response.total_matches;
        assert!(!response.results.is_empty());
    }
    let report = format!(
        "{} chunks; {}; {last_total} matches counted",
        chunks,
        summary(&timings)
    );
    assert_eq!(timings.len(), RUNS, "{report}");
    let median = percentile(&timings, 50);
    let p95 = percentile(&timings, 95);
    assert!(
        median < MEDIAN_BOUND,
        "median over {MEDIAN_BOUND:?}: {report}"
    );
    assert!(p95 < P95_BOUND, "p95 over {P95_BOUND:?}: {report}");
}
