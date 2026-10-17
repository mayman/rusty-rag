//! Hit policy: from ranked candidates to the hits shown. At most
//! `MAX_HITS_PER_FILE` hits per file, then the first `k`.

use std::collections::BTreeMap;

use crate::domain::repo_path::RepoPath;
use crate::domain::scored_chunk::ScoredChunk;
use crate::domain::source_id::SourceId;

/// Most hits from one file (one path in one source).
pub const MAX_HITS_PER_FILE: usize = 2;

/// Sorts `candidates` into ranking order, keeps at most `MAX_HITS_PER_FILE` per
/// file (the best ones), and returns the first `k`.
pub fn select_hits(candidates: Vec<ScoredChunk>, k: usize) -> Vec<ScoredChunk> {
    let mut candidates = candidates;
    candidates.sort_by(ScoredChunk::ranking_order);
    let mut per_file: BTreeMap<(SourceId, RepoPath), usize> = BTreeMap::new();
    candidates
        .into_iter()
        .filter(|hit| {
            let seen = per_file
                .entry((hit.chunk.source.clone(), hit.chunk.path.clone()))
                .or_insert(0);
            *seen += 1;
            *seen <= MAX_HITS_PER_FILE
        })
        .take(k)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::select_hits;
    use crate::domain::scored_chunk::fixtures::scored;

    fn paths(hits: &[crate::domain::scored_chunk::ScoredChunk]) -> Vec<String> {
        hits.iter()
            .map(|h| format!("{}:{}", h.chunk.path, h.chunk.lines.start()))
            .collect()
    }

    #[test]
    fn keeps_the_two_best_hits_per_file() {
        let hits = select_hits(
            vec![
                scored("a.rs", 1, 5.0),
                scored("a.rs", 10, 4.0),
                scored("a.rs", 20, 3.0),
                scored("b.rs", 1, 1.0),
            ],
            10,
        );
        assert_eq!(paths(&hits), ["a.rs:1", "a.rs:10", "b.rs:1"]);
    }

    #[test]
    fn returns_at_most_k_hits() {
        let hits = select_hits(
            vec![
                scored("a.rs", 1, 3.0),
                scored("b.rs", 1, 2.0),
                scored("c.rs", 1, 1.0),
            ],
            2,
        );
        assert_eq!(paths(&hits), ["a.rs:1", "b.rs:1"]);
    }

    #[test]
    fn sorts_unsorted_candidates_deterministically() {
        let a = vec![
            scored("b.rs", 1, 1.0),
            scored("a.rs", 1, 1.0),
            scored("c.rs", 1, 2.0),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(select_hits(a, 10), select_hits(b, 10));
    }

    #[test]
    fn capped_hits_free_room_for_lower_ranked_files() {
        let hits = select_hits(
            vec![
                scored("a.rs", 1, 9.0),
                scored("a.rs", 2, 8.0),
                scored("a.rs", 3, 7.0),
                scored("b.rs", 1, 1.0),
            ],
            3,
        );
        assert_eq!(paths(&hits), ["a.rs:1", "a.rs:2", "b.rs:1"]);
    }

    #[test]
    fn zero_k_returns_nothing() {
        assert!(select_hits(vec![scored("a.rs", 1, 1.0)], 0).is_empty());
    }
}
