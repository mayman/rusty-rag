//! Retrieval: the search policy applied around a `ChunkSearcher`.
//!
//! Run `And`; when it matches nothing and the query has several words, run
//! `OrFallback` and report that mode. The page is `offset` to `offset + k` of the
//! hits that survive the per-file cap. One more hit is selected than the page needs
//! (`offset + k + 1`) so `has_more` is exact.
//!
//! Each mode costs one searcher call. A search ranks every match whatever the limit
//! is, so asking for a small limit and then widening it would only repeat that work:
//! the searcher is asked once for `MAX_CANDIDATES` candidates (one more, as a probe
//! that tells whether the searcher had matches left). If the ceiling stops the
//! selection short of a hit past the page and candidates were left unexamined,
//! `has_more` is true: a next page may exist, and saying so is safer than hiding it.

use std::collections::BTreeMap;

use crate::application::error::StorageError;
use crate::application::ports::{ChunkSearcher, SearchOutcome};
use crate::domain::hit_policy::select_hits;
use crate::domain::query::{Query, QueryMode};
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::IndexedRevision;
use crate::domain::source_id::SourceId;

/// The most candidates a single search examines (more only when one page needs more).
pub const MAX_CANDIDATES: usize = 5000;

/// One page of hits and what is known about the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct Retrieval {
    /// The mode that produced the hits (the last mode tried when there are none).
    pub mode: QueryMode,
    /// The page: at most `k` hits after the per-file cap, starting at `offset`.
    pub hits: Vec<ScoredChunk>,
    /// Whether at least one more hit follows the page.
    pub has_more: bool,
    /// Chunks that matched in `mode`, before the per-file cap and paging, counted
    /// up to `TOTAL_MATCHES_CAP`. A lower bound when `total_is_exact` is false.
    pub total_matches: usize,
    /// Whether `total_matches` is the exact count.
    pub total_is_exact: bool,
    /// Revisions of the sources that appear in the searcher's hits.
    pub revisions: BTreeMap<SourceId, IndexedRevision>,
}

/// Searches with AND, then OR, applies the hit policy, and returns the page
/// `[offset, offset + k)` of the hits.
pub fn retrieve(
    searcher: &dyn ChunkSearcher,
    query: &Query,
    filters: &SearchFilters,
    offset: usize,
    k: usize,
) -> Result<Retrieval, StorageError> {
    let mut mode = QueryMode::And;
    if k == 0 {
        return Ok(Retrieval {
            mode,
            hits: Vec::new(),
            has_more: false,
            total_matches: 0,
            total_is_exact: true,
            revisions: BTreeMap::new(),
        });
    }
    let wanted = offset.saturating_add(k).saturating_add(1);
    loop {
        let examined = examine(searcher, query, mode, filters, wanted)?;
        match query.fallback_after(mode) {
            Some(next) if examined.total_matches == 0 => mode = next,
            _ => {
                // A hit follows the page when one was selected past it. When the
                // search stopped at the ceiling with candidates left unexamined,
                // more hits cannot be ruled out, so claim them rather than hide
                // a next page.
                let has_more =
                    examined.selected.len() > offset.saturating_add(k) || !examined.exhausted;
                return Ok(Retrieval {
                    mode,
                    hits: examined.selected.into_iter().skip(offset).take(k).collect(),
                    has_more,
                    total_matches: examined.total_matches,
                    total_is_exact: examined.total_is_exact,
                    revisions: examined.revisions,
                });
            }
        }
    }
}

/// What one searcher call settled on.
struct Examined {
    /// Up to `wanted` hits after the per-file cap, best first.
    selected: Vec<ScoredChunk>,
    /// Whether the searcher had no match beyond the examined candidates.
    exhausted: bool,
    total_matches: usize,
    total_is_exact: bool,
    revisions: BTreeMap<SourceId, IndexedRevision>,
}

/// Up to `wanted` hits after the per-file cap, from one search of at most
/// `MAX_CANDIDATES` candidates.
fn examine(
    searcher: &dyn ChunkSearcher,
    query: &Query,
    mode: QueryMode,
    filters: &SearchFilters,
    wanted: usize,
) -> Result<Examined, StorageError> {
    let ceiling = MAX_CANDIDATES.max(wanted);
    // One candidate beyond the ceiling is a probe: if it comes back, matches were
    // left unexamined. The total cannot tell, because it is capped.
    let SearchOutcome {
        mut hits,
        revisions,
        total_matches,
        total_is_exact,
    } = searcher.search(query, mode, filters, ceiling.saturating_add(1))?;
    let exhausted = hits.len() <= ceiling;
    hits.truncate(ceiling);
    Ok(Examined {
        selected: select_hits(hits, wanted),
        exhausted,
        total_matches,
        total_is_exact,
        revisions,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::{retrieve, Retrieval, MAX_CANDIDATES};
    use crate::application::error::StorageError;
    use crate::application::ports::{ChunkSearcher, SearchOutcome, TOTAL_MATCHES_CAP};
    use crate::domain::index_versions::IndexVersions;
    use crate::domain::indexed_at::IndexedAt;
    use crate::domain::oid::CommitId;
    use crate::domain::query::{Query, QueryMode};
    use crate::domain::scored_chunk::fixtures::scored;
    use crate::domain::scored_chunk::ScoredChunk;
    use crate::domain::search_filters::SearchFilters;
    use crate::domain::source::IndexedRevision;

    /// Answers each mode from a fixed, ranked script (honouring `limit`, reporting
    /// the full match count) and records the calls.
    struct Scripted {
        and_hits: Vec<ScoredChunk>,
        or_hits: Vec<ScoredChunk>,
        calls: Mutex<Vec<(QueryMode, usize)>>,
    }

    impl Scripted {
        fn new(and_hits: Vec<ScoredChunk>, or_hits: Vec<ScoredChunk>) -> Self {
            Self {
                and_hits,
                or_hits,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(QueryMode, usize)> {
            self.calls.lock().unwrap().clone()
        }

        fn limits(&self) -> Vec<usize> {
            self.calls().into_iter().map(|(_, limit)| limit).collect()
        }
    }

    impl ChunkSearcher for Scripted {
        fn search(
            &self,
            _query: &Query,
            mode: QueryMode,
            _filters: &SearchFilters,
            limit: usize,
        ) -> Result<SearchOutcome, StorageError> {
            self.calls.lock().unwrap().push((mode, limit));
            let all = match mode {
                QueryMode::And => &self.and_hits,
                QueryMode::OrFallback => &self.or_hits,
            };
            let mut revisions = BTreeMap::new();
            if let Some(first) = all.first() {
                revisions.insert(
                    first.chunk.source.clone(),
                    IndexedRevision {
                        commit: CommitId::parse(&"ab".repeat(20)).unwrap(),
                        at: IndexedAt::from_epoch_seconds(5),
                        versions: IndexVersions::current(),
                    },
                );
            }
            Ok(SearchOutcome {
                hits: all.iter().take(limit).cloned().collect(),
                revisions,
                total_matches: all.len().min(TOTAL_MATCHES_CAP),
                total_is_exact: all.len() <= TOTAL_MATCHES_CAP,
            })
        }
    }

    fn run(searcher: &Scripted, text: &str, offset: usize, k: usize) -> Retrieval {
        retrieve(
            searcher,
            &Query::parse(text).unwrap(),
            &SearchFilters::default(),
            offset,
            k,
        )
        .unwrap()
    }

    /// `n` distinct files, best first.
    fn files(n: usize) -> Vec<ScoredChunk> {
        (0..n)
            .map(|i| scored(&format!("f{i:03}.rs"), 1, 1000.0 - i as f64))
            .collect()
    }

    /// `n` chunks of one file, best first.
    fn one_file(path: &str, n: usize, top_score: f64) -> Vec<ScoredChunk> {
        (0..n)
            .map(|i| scored(path, i + 1, top_score - i as f64 * 0.001))
            .collect()
    }

    fn names(hits: &[ScoredChunk]) -> Vec<String> {
        hits.iter()
            .map(|h| format!("{}:{}", h.chunk.path, h.chunk.lines.start()))
            .collect()
    }

    #[test]
    fn and_hits_are_returned_without_running_or() {
        let searcher = Scripted::new(vec![scored("a.rs", 1, 1.0)], vec![scored("b.rs", 1, 1.0)]);
        let result = run(&searcher, "alpha beta", 0, 5);
        assert_eq!(result.mode, QueryMode::And);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(searcher.calls().len(), 1);
    }

    #[test]
    fn falls_back_to_or_when_and_matches_nothing() {
        let searcher = Scripted::new(vec![], vec![scored("b.rs", 1, 1.0)]);
        let result = run(&searcher, "alpha beta", 0, 5);
        assert_eq!(result.mode, QueryMode::OrFallback);
        assert_eq!(result.hits[0].chunk.path.as_str(), "b.rs");
    }

    #[test]
    fn single_word_queries_never_run_or() {
        let searcher = Scripted::new(vec![], vec![scored("b.rs", 1, 1.0)]);
        let result = run(&searcher, "alpha", 0, 5);
        assert_eq!(result.mode, QueryMode::And);
        assert!(result.hits.is_empty());
        assert_eq!(searcher.calls().len(), 1);
    }

    #[test]
    fn reports_or_fallback_even_when_it_finds_nothing_too() {
        let searcher = Scripted::new(vec![], vec![]);
        let result = run(&searcher, "alpha beta", 0, 5);
        assert_eq!(result.mode, QueryMode::OrFallback);
        assert!(result.hits.is_empty());
        assert_eq!(result.total_matches, 0);
        assert!(!result.has_more);
    }

    #[test]
    fn asks_the_searcher_once_for_the_candidate_ceiling_plus_one_probe() {
        let searcher = Scripted::new(vec![scored("a.rs", 1, 1.0)], vec![]);
        run(&searcher, "alpha", 4, 3);
        assert_eq!(
            searcher.calls(),
            [(QueryMode::And, MAX_CANDIDATES + 1)],
            "one call; the extra candidate only tells whether the searcher had more"
        );
    }

    #[test]
    fn applies_the_per_file_cap_and_k() {
        let candidates = vec![
            scored("a.rs", 1, 5.0),
            scored("a.rs", 2, 4.0),
            scored("a.rs", 3, 3.0),
            scored("b.rs", 1, 2.0),
        ];
        let searcher = Scripted::new(candidates, vec![]);
        let result = run(&searcher, "alpha", 0, 3);
        assert_eq!(names(&result.hits), ["a.rs:1", "a.rs:2", "b.rs:1"]);
    }

    #[test]
    fn zero_k_returns_nothing_without_searching() {
        let searcher = Scripted::new(vec![scored("a.rs", 1, 1.0)], vec![]);
        let result = run(&searcher, "alpha", 0, 0);
        assert!(result.hits.is_empty());
        assert!(!result.has_more);
        assert!(searcher.calls().is_empty());
    }

    #[test]
    fn offset_skips_hits_after_the_cap() {
        let searcher = Scripted::new(files(6), vec![]);
        let result = run(&searcher, "alpha", 2, 2);
        assert_eq!(names(&result.hits), ["f002.rs:1", "f003.rs:1"]);
        assert!(result.has_more, "f004 and f005 follow");
    }

    #[test]
    fn has_more_is_true_only_when_a_hit_follows_the_page() {
        let exactly_k = Scripted::new(files(3), vec![]);
        assert!(!run(&exactly_k, "alpha", 0, 3).has_more);
        let one_more = Scripted::new(files(4), vec![]);
        assert!(run(&one_more, "alpha", 0, 3).has_more);
        let last_page = Scripted::new(files(5), vec![]);
        assert!(!run(&last_page, "alpha", 3, 2).has_more);
    }

    #[test]
    fn has_more_counts_hits_after_the_cap_not_raw_candidates() {
        let searcher = Scripted::new(one_file("a.rs", 3, 9.0), vec![]);
        let result = run(&searcher, "alpha", 0, 2);
        assert_eq!(result.hits.len(), 2);
        assert!(
            !result.has_more,
            "the third chunk of a.rs is dropped by the cap, so nothing follows"
        );
    }

    #[test]
    fn an_offset_past_the_end_gives_an_empty_page_in_the_mode_that_matched() {
        let searcher = Scripted::new(files(3), vec![scored("z.rs", 1, 1.0)]);
        let result = run(&searcher, "alpha beta", 10, 5);
        assert!(result.hits.is_empty());
        assert!(!result.has_more);
        assert_eq!(result.mode, QueryMode::And, "AND matched, so no fallback");
        assert_eq!(result.total_matches, 3);
    }

    #[test]
    fn a_page_the_file_cap_leaves_short_is_not_retried_with_more_candidates() {
        let mut ranked = one_file("a.rs", 100, 500.0);
        ranked.push(scored("b.rs", 1, 1.0));
        let searcher = Scripted::new(ranked, vec![]);
        let result = run(&searcher, "alpha", 0, 3);
        assert_eq!(names(&result.hits), ["a.rs:1", "a.rs:2", "b.rs:1"]);
        assert!(!result.has_more);
        assert_eq!(searcher.limits().len(), 1, "no widening loop");
    }

    #[test]
    fn a_ceiling_that_stops_the_search_with_matches_left_reports_more() {
        let searcher = Scripted::new(one_file("a.rs", MAX_CANDIDATES + 500, 9000.0), vec![]);
        let result = run(&searcher, "alpha", 0, 2);
        assert_eq!(result.hits.len(), 2);
        assert!(
            result.has_more,
            "500 candidates were never examined, so more hits cannot be ruled out"
        );
        assert_eq!(searcher.limits().len(), 1);
    }

    #[test]
    fn has_more_is_exact_when_the_ceiling_exactly_exhausts_the_searcher() {
        let searcher = Scripted::new(one_file("a.rs", MAX_CANDIDATES, 9000.0), vec![]);
        let result = run(&searcher, "alpha", 0, 2);
        assert!(
            !result.has_more,
            "every match was examined and the cap dropped them all, even though the \
             reported total is only a lower bound"
        );
    }

    #[test]
    fn has_more_at_the_ceiling_is_still_true_when_a_hit_follows_the_page() {
        let mut ranked = files(4);
        ranked.extend(one_file("z.rs", MAX_CANDIDATES, 1.0));
        let searcher = Scripted::new(ranked, vec![]);
        assert!(run(&searcher, "alpha", 0, 3).has_more);
    }

    #[test]
    fn a_huge_page_still_gets_at_least_the_hits_it_asks_for() {
        let searcher = Scripted::new(files(1), vec![]);
        run(&searcher, "alpha", 0, MAX_CANDIDATES * 2);
        assert!(searcher.limits()[0] > MAX_CANDIDATES * 2);
    }

    #[test]
    fn totals_and_revisions_come_from_the_search_that_produced_the_hits() {
        let searcher = Scripted::new(files(12), vec![]);
        let result = run(&searcher, "alpha", 0, 3);
        assert_eq!(
            result.total_matches, 12,
            "matches before the page and the cap"
        );
        assert!(result.total_is_exact);
        assert_eq!(result.revisions.len(), 1);
    }

    #[test]
    fn a_total_above_the_searcher_cap_is_a_lower_bound() {
        let searcher = Scripted::new(files(TOTAL_MATCHES_CAP + 200), vec![]);
        let result = run(&searcher, "alpha", 0, 3);
        assert_eq!(result.total_matches, TOTAL_MATCHES_CAP);
        assert!(!result.total_is_exact, "at least {TOTAL_MATCHES_CAP}");
        assert!(result.has_more);
    }

    #[test]
    fn a_zero_k_retrieval_reports_an_exact_zero() {
        let searcher = Scripted::new(files(3), vec![]);
        let result = run(&searcher, "alpha", 0, 0);
        assert_eq!(result.total_matches, 0);
        assert!(result.total_is_exact);
    }
}
