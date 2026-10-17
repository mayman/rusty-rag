//! ScoredChunk: a chunk found by a search, with its relevance score.

use std::cmp::Ordering;

use crate::domain::chunk::Chunk;

/// A search hit. `score` is higher for better matches and comparable only within
/// one search.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredChunk {
    pub chunk: Chunk,
    pub score: f64,
}

impl ScoredChunk {
    /// The canonical result order: best score first, ties broken by source, path,
    /// first line, then chunk id, so equal scores never reorder between runs.
    /// Searchers return hits in this order.
    pub fn ranking_order(a: &ScoredChunk, b: &ScoredChunk) -> Ordering {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.chunk.source.cmp(&b.chunk.source))
            .then_with(|| a.chunk.path.cmp(&b.chunk.path))
            .then_with(|| a.chunk.lines.start().cmp(&b.chunk.lines.start()))
            .then_with(|| a.chunk.id.cmp(&b.chunk.id))
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::ScoredChunk;
    use crate::domain::chunk::Chunk;
    use crate::domain::chunk_id::ChunkId;
    use crate::domain::git_location::GitLocation;
    use crate::domain::language::Language;
    use crate::domain::line_range::LineRange;
    use crate::domain::repo_path::RepoPath;
    use crate::domain::source_id::SourceId;

    pub fn scored(path: &str, start: usize, score: f64) -> ScoredChunk {
        let path = RepoPath::parse(path).unwrap();
        let id = blake3::hash(format!("{path}:{start}").as_bytes())
            .to_hex()
            .to_string();
        ScoredChunk {
            chunk: Chunk {
                id: ChunkId::parse(&id).unwrap(),
                source: SourceId::from_location(
                    &GitLocation::parse("https://github.com/o/r").unwrap(),
                )
                .unwrap(),
                language: Language::from_path(&path),
                path,
                lines: LineRange::new(start, start + 1).unwrap(),
                heading_path: Vec::new(),
                text: "t".to_owned(),
            },
            score,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::fixtures::scored;
    use super::ScoredChunk;

    #[test]
    fn higher_scores_sort_first() {
        let high = scored("a.rs", 1, 2.0);
        let low = scored("a.rs", 1, 1.0);
        assert_eq!(ScoredChunk::ranking_order(&high, &low), Ordering::Less);
    }

    #[test]
    fn equal_scores_sort_by_path_then_first_line() {
        let a = scored("a.rs", 9, 1.0);
        let b = scored("b.rs", 1, 1.0);
        let a_early = scored("a.rs", 1, 1.0);
        assert_eq!(ScoredChunk::ranking_order(&a, &b), Ordering::Less);
        assert_eq!(ScoredChunk::ranking_order(&a_early, &a), Ordering::Less);
    }

    #[test]
    fn identical_chunks_compare_equal() {
        let a = scored("a.rs", 1, 1.0);
        assert_eq!(ScoredChunk::ranking_order(&a, &a.clone()), Ordering::Equal);
    }
}
