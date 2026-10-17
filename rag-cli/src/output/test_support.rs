//! Builders for output tests: valid domain values with fixed ids and commits.

use std::collections::BTreeMap;

use rag_core::application::retrieval::Retrieval;
use rag_core::domain::chunk::Chunk;
use rag_core::domain::chunk_id::ChunkId;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::language::Language;
use rag_core::domain::line_range::LineRange;
use rag_core::domain::oid::CommitId;
use rag_core::domain::query::QueryMode;
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::scored_chunk::ScoredChunk;
use rag_core::domain::source::IndexedRevision;
use rag_core::domain::source_id::SourceId;

/// The commit every fixture source is indexed at.
pub const COMMIT: &str = "abababababababababababababababababababab";
/// `2023-11-14T22:13:20Z`.
pub const INDEXED_AT: i64 = 1_700_000_000;

/// The source `github.com/o/r`.
pub fn source() -> SourceId {
    SourceId::from_location(&GitLocation::parse("https://github.com/o/r").unwrap()).unwrap()
}

pub fn revision() -> IndexedRevision {
    IndexedRevision {
        commit: CommitId::parse(COMMIT).unwrap(),
        at: IndexedAt::from_epoch_seconds(INDEXED_AT),
        versions: IndexVersions::current(),
    }
}

/// A chunk of `path` whose text starts at `first_line`. `n` makes the id distinct.
pub fn chunk(n: u64, path: &str, first_line: usize, headings: &[&str], text: &str) -> Chunk {
    let path = RepoPath::parse(path).unwrap();
    let last_line = first_line + text.lines().count().max(1) - 1;
    Chunk {
        id: ChunkId::parse(&format!("{n:064x}")).unwrap(),
        source: source(),
        language: Language::from_path(&path),
        path,
        lines: LineRange::new(first_line, last_line).unwrap(),
        heading_path: headings.iter().map(|h| (*h).to_owned()).collect(),
        text: text.to_owned(),
    }
}

pub fn hit(n: u64, path: &str, first_line: usize, text: &str, score: f64) -> ScoredChunk {
    ScoredChunk {
        chunk: chunk(n, path, first_line, &[], text),
        score,
    }
}

/// A retrieval page with the fixture revision for its source.
pub fn retrieval(
    mode: QueryMode,
    hits: Vec<ScoredChunk>,
    total: usize,
    has_more: bool,
) -> Retrieval {
    let mut revisions = BTreeMap::new();
    if !hits.is_empty() {
        revisions.insert(source(), revision());
    }
    Retrieval {
        mode,
        hits,
        has_more,
        total_matches: total,
        revisions,
    }
}
