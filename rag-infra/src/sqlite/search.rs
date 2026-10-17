//! `ChunkSearcher` over `chunks_fts`: one read transaction per search, ranking by
//! bm25 with the column weights pinned in `schema::COLUMN_WEIGHTS`, scores negated so
//! that higher is better, filters applied before the limit.

use std::collections::BTreeMap;

use rag_core::application::error::StorageError;
use rag_core::application::ports::{ChunkSearcher, SearchOutcome};
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::query::{Query, QueryMode};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::scored_chunk::ScoredChunk;
use rag_core::domain::search_filters::SearchFilters;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

use super::error::storage_error;
use super::fts_query::match_expression;
use super::records::{load_source, RawChunk};
use super::schema::COLUMN_WEIGHTS;
use super::SqliteStore;

/// Index of the path column in a hit row (see `RawChunk::read`).
const PATH_COLUMN: usize = 2;
/// Index of the score column in a hit row, after the chunk columns.
const SCORE_COLUMN: usize = 8;

impl ChunkSearcher for SqliteStore {
    fn search(
        &self,
        query: &Query,
        mode: QueryMode,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<SearchOutcome, StorageError> {
        let expression = match_expression(query, mode);
        self.read_snapshot(|tx| {
            ensure_touched_sources_are_current(tx, filters)?;
            let (hits, total_matches) = ranked_hits(tx, &expression, filters, limit)?;
            let mut revisions = BTreeMap::new();
            for hit in &hits {
                let id = &hit.chunk.source;
                if revisions.contains_key(id) {
                    continue;
                }
                let revision = load_source(tx, id)?.and_then(|s| s.indexed().cloned());
                if let Some(revision) = revision {
                    revisions.insert(id.clone(), revision);
                }
            }
            Ok(SearchOutcome {
                hits,
                revisions,
                total_matches,
            })
        })
    }
}

/// `ReindexRequired` when a searchable source the query touches was indexed with
/// other tokenizer, chunker, or policy versions.
fn ensure_touched_sources_are_current(
    connection: &Connection,
    filters: &SearchFilters,
) -> Result<(), StorageError> {
    let current = IndexVersions::current();
    let mut sql = String::from(
        "SELECT 1 FROM sources WHERE indexed_commit IS NOT NULL AND status <> 'removing' \
         AND NOT (tokenizer_version = ? AND chunker_version = ? AND policy_version = ?)",
    );
    let mut params = vec![
        Value::Integer(i64::from(current.tokenizer)),
        Value::Integer(i64::from(current.chunker)),
        Value::Integer(i64::from(current.policy)),
    ];
    push_in_list(&mut sql, &mut params, "source_id", source_names(filters));
    sql.push_str(" LIMIT 1");
    let stale = connection
        .prepare(&sql)
        .map_err(storage_error)?
        .exists(params_from_iter(params))
        .map_err(storage_error)?;
    if stale {
        Err(StorageError::ReindexRequired)
    } else {
        Ok(())
    }
}

/// The best `limit` matching chunks in ranking order and the number of all matches.
///
/// One pass over the matches in rank order: the source and language filters are SQL
/// conditions, the path glob is checked per row, and only chunks that pass every
/// filter are counted or kept, so no filter ever applies after the limit.
fn ranked_hits(
    connection: &Connection,
    expression: &str,
    filters: &SearchFilters,
    limit: usize,
) -> Result<(Vec<ScoredChunk>, usize), StorageError> {
    let [path, heading, body] = COLUMN_WEIGHTS;
    let mut sql = format!(
        "SELECT c.chunk_id, c.source_id, c.path, c.language, c.start_line, c.end_line, \
         c.heading_path, c.text, -bm25(chunks_fts, {path:?}, {heading:?}, {body:?}) AS score \
         FROM chunks_fts \
         JOIN chunks c ON c.chunk_rowid = chunks_fts.rowid \
         JOIN sources s ON s.source_id = c.source_id \
         WHERE chunks_fts MATCH ? AND s.indexed_commit IS NOT NULL AND s.status <> 'removing'"
    );
    let mut params = vec![Value::Text(expression.to_owned())];
    push_in_list(&mut sql, &mut params, "c.source_id", source_names(filters));
    push_in_list(
        &mut sql,
        &mut params,
        "c.language",
        filters
            .languages
            .iter()
            .map(|language| language.name().to_owned())
            .collect(),
    );
    sql.push_str(" ORDER BY score DESC, c.source_id, c.path, c.start_line, c.chunk_id");

    let mut statement = connection.prepare(&sql).map_err(storage_error)?;
    let mut rows = statement
        .query(params_from_iter(params))
        .map_err(storage_error)?;
    let mut hits = Vec::new();
    let mut total = 0;
    while let Some(row) = rows.next().map_err(storage_error)? {
        if let Some(glob) = &filters.path {
            let stored: String = row.get(PATH_COLUMN).map_err(storage_error)?;
            match RepoPath::from_git(&stored) {
                Ok(path) if glob.matches(&path) => {}
                _ => continue,
            }
        }
        total += 1;
        if hits.len() < limit {
            let score: f64 = row.get(SCORE_COLUMN).map_err(storage_error)?;
            let chunk = RawChunk::read(row).map_err(storage_error)?.into_chunk()?;
            // `+ 0.0` turns a negated zero into a plain one.
            hits.push(ScoredChunk {
                chunk,
                score: score + 0.0,
            });
        }
    }
    Ok((hits, total))
}

fn source_names(filters: &SearchFilters) -> Vec<String> {
    filters
        .sources
        .iter()
        .map(|source| source.as_str().to_owned())
        .collect()
}

/// Appends `AND column IN (?, ?, ...)` for a non-empty list of values.
fn push_in_list(sql: &mut String, params: &mut Vec<Value>, column: &str, values: Vec<String>) {
    if values.is_empty() {
        return;
    }
    let placeholders = vec!["?"; values.len()].join(", ");
    sql.push_str(&format!(" AND {column} IN ({placeholders})"));
    params.extend(values.into_iter().map(Value::Text));
}
