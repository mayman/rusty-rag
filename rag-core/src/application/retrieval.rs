//! Retrieval: the search policy applied around a `ChunkSearcher`.
//!
//! Run `And`; when it matches nothing and the query has several words, run
//! `OrFallback` and report that mode. The page is `offset` to `offset + k` of the
//! hits that survive the per-file cap. One more hit is selected than the page needs
//! (`offset + k + 1`) so `has_more` is exact. Because the cap can discard many of
//! the searcher's candidates, the candidate limit starts at `CANDIDATES_PER_HIT`
//! per wanted hit and doubles, up to `MAX_CANDIDATES`, while the cap leaves the
//! selection short and the searcher has more matches to give. If the ceiling stops
//! the search with matches left unexamined and no hit past the page, `has_more` is
//! true: a next page may exist, and saying so is safer than hiding it.

use std::collections::BTreeMap;

use crate::application::error::StorageError;
use crate::application::ports::{ChunkSearcher, SearchOutcome};
use crate::domain::hit_policy::select_hits;
use crate::domain::query::{Query, QueryMode};
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::search_filters::SearchFilters;
use crate::domain::source::IndexedRevision;
use crate::domain::source_id::SourceId;

/// Candidates requested from the searcher per wanted hit, on the first attempt.
pub const CANDIDATES_PER_HIT: usize = 10;
/// The most candidates a single search asks for (more only when one page needs more).
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
    /// Chunks that matched in `mode`, before the per-file cap and paging.
    pub total_matches: usize,
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
            revisions: BTreeMap::new(),
        });
    }
    let wanted = offset.saturating_add(k).saturating_add(1);
    loop {
        let widened = select_widening(searcher, query, mode, filters, wanted)?;
        match query.fallback_after(mode) {
            Some(next) if widened.total_matches == 0 => mode = next,
            _ => {
                // A hit follows the page when one was selected past it. When the
                // search stopped at the ceiling with candidates left unexamined,
                // more hits cannot be ruled out, so claim them rather than hide
                // a next page.
                let has_more =
                    widened.selected.len() > offset.saturating_add(k) || !widened.exhausted;
                return Ok(Retrieval {
                    mode,
                    hits: widened.selected.into_iter().skip(offset).take(k).collect(),
                    has_more,
                    total_matches: widened.total_matches,
                    revisions: widened.revisions,
                });
            }
        }
    }
}

/// What the widening search settled on.
struct Widened {
    /// Up to `wanted` hits after the per-file cap, best first.
    selected: Vec<ScoredChunk>,
    /// Whether the searcher returned every one of its matches (nothing was left
    /// unexamined behind the candidate limit).
    exhausted: bool,
    total_matches: usize,
    revisions: BTreeMap<SourceId, IndexedRevision>,
}

/// Up to `wanted` hits after the per-file cap. Doubles the candidate limit while
/// the cap leaves fewer than `wanted` and the searcher has more to give.
fn select_widening(
    searcher: &dyn ChunkSearcher,
    query: &Query,
    mode: QueryMode,
    filters: &SearchFilters,
    wanted: usize,
) -> Result<Widened, StorageError> {
    let ceiling = MAX_CANDIDATES.max(wanted);
    let mut limit = wanted.saturating_mul(CANDIDATES_PER_HIT).min(ceiling);
    loop {
        let SearchOutcome {
            hits,
            revisions,
            total_matches,
        } = searcher.search(query, mode, filters, limit)?;
        let exhausted = hits.len() < limit || hits.len() >= total_matches;
        let selected = select_hits(hits, wanted);
        if selected.len() >= wanted || exhausted || limit >= ceiling {
            return Ok(Widened {
                selected,
                exhausted,
                total_matches,
                revisions,
            });
        }
        limit = limit.saturating_mul(2).min(ceiling);
    }
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::{retrieve, Retrieval, CANDIDATES_PER_HIT, MAX_CANDIDATES};
    use crate::application::error::StorageError;
    use crate::application::ports::{ChunkSearcher, SearchOutcome};
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
                total_matches: all.len(),
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
    fn asks_for_the_page_plus_one_hit_with_headroom_for_the_file_cap() {
        let searcher = Scripted::new(vec![scored("a.rs", 1, 1.0)], vec![]);
        run(&searcher, "alpha", 4, 3);
        assert_eq!(
            searcher.calls()[0],
            (QueryMode::And, (4 + 3 + 1) * CANDIDATES_PER_HIT)
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
    fn widens_the_candidate_limit_while_the_cap_leaves_the_page_short() {
        let mut ranked = one_file("a.rs", 100, 500.0);
        ranked.push(scored("b.rs", 1, 1.0));
        let searcher = Scripted::new(ranked, vec![]);
        let result = run(&searcher, "alpha", 0, 3);
        assert_eq!(names(&result.hits), ["a.rs:1", "a.rs:2", "b.rs:1"]);
        assert!(!result.has_more);
        assert_eq!(
            searcher.limits(),
            [40, 80, 160],
            "doubles from (offset + k + 1) * {CANDIDATES_PER_HIT} until b.rs is reached"
        );
    }

    #[test]
    fn does_not_widen_once_the_searcher_is_exhausted() {
        let searcher = Scripted::new(one_file("a.rs", 5, 9.0), vec![]);
        let result = run(&searcher, "alpha", 0, 3);
        assert_eq!(result.hits.len(), 2);
        assert_eq!(searcher.limits(), [40], "all 5 matches came back at once");
    }

    #[test]
    fn widening_stops_at_the_ceiling() {
        let searcher = Scripted::new(one_file("a.rs", MAX_CANDIDATES + 500, 9000.0), vec![]);
        let result = run(&searcher, "alpha", 0, 2);
        assert_eq!(result.hits.len(), 2);
        assert!(
            result.has_more,
            "500 candidates were never examined, so more hits cannot be ruled out"
        );
        let limits = searcher.limits();
        assert_eq!(limits.last(), Some(&MAX_CANDIDATES));
        assert!(limits.windows(2).all(|w| w[1] > w[0]), "{limits:?}");
        assert!(limits.iter().all(|l| *l <= MAX_CANDIDATES), "{limits:?}");
    }

    #[test]
    fn has_more_is_exact_when_the_ceiling_exactly_exhausts_the_searcher() {
        let searcher = Scripted::new(one_file("a.rs", MAX_CANDIDATES, 9000.0), vec![]);
        let result = run(&searcher, "alpha", 0, 2);
        assert_eq!(searcher.limits().last(), Some(&MAX_CANDIDATES));
        assert!(
            !result.has_more,
            "every match was examined and the cap dropped them all"
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
        assert_eq!(result.revisions.len(), 1);
    }
}
