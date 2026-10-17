//! `ChunkSearcher` over `chunks_fts`: one read transaction per search, ranking by
//! bm25 with the column weights pinned in `schema::COLUMN_WEIGHTS`, scores negated so
//! that higher is better and scaled by the path prior (`domain::path_prior`), filters
//! applied before the limit.
//!
//! The ranked query is narrow on purpose. It selects the row id, the score, and the
//! sort keys, never the chunk text, so ranking a common term over a large index
//! moves a few dozen bytes per match through the sorter instead of whole chunks.
//! `ORDER BY ... LIMIT` keeps only the best `limit` rows. The text, heading, and the
//! other columns are then fetched for those winners alone, by row id. The number of
//! matches comes from a separate counting subquery that stops at
//! `TOTAL_MATCHES_CAP + 1`.

use std::collections::BTreeMap;

use rag_core::application::error::StorageError;
use rag_core::application::ports::{ChunkSearcher, SearchOutcome, TOTAL_MATCHES_CAP};
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::path_glob::PathGlob;
use rag_core::domain::path_prior::path_prior;
use rag_core::domain::query::{Query, QueryMode};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::scored_chunk::ScoredChunk;
use rag_core::domain::search_filters::SearchFilters;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

use super::error::storage_error;
use super::fts_query::match_expression;
use super::records::{load_source, RawChunk, CHUNK_COLUMNS};
use super::schema::COLUMN_WEIGHTS;
use super::SqliteStore;

/// The SQL function that applies the domain `PathGlob` to a stored path.
const PATH_GLOB_FUNCTION: &str = "rag_path_glob";

/// The SQL function that applies the domain `path_prior` to a stored path.
const PATH_PRIOR_FUNCTION: &str = "rag_path_prior";

/// Registers `rag_path_glob(pattern, path)` on a read connection. The pattern is
/// parsed once per statement (the argument is cached as auxiliary data), the path
/// is checked with `PathGlob::matches`, so the glob is a domain rule even though SQL
/// evaluates it.
pub(super) fn register_path_glob(connection: &Connection) -> rusqlite::Result<()> {
    connection.create_scalar_function(
        PATH_GLOB_FUNCTION,
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let glob = context.get_or_create_aux(0, |pattern| {
                let pattern = pattern.as_str().map_err(|e| e.to_string())?;
                PathGlob::parse(pattern).map_err(|_| "the path glob is empty".to_owned())
            })?;
            let path = context.get_raw(1).as_str()?;
            Ok(RepoPath::from_git(path).is_ok_and(|path| glob.matches(&path)))
        },
    )
}

/// Registers `rag_path_prior(path)`: the multiplier of `domain::path_prior` (1.0 for
/// a stored path that is not a safe `RepoPath`). Cheap: string comparisons on a
/// path, evaluated once per matching row inside the ranking query, so the prior
/// shapes the order before `LIMIT` keeps the best rows.
pub(super) fn register_path_prior(connection: &Connection) -> rusqlite::Result<()> {
    connection.create_scalar_function(
        PATH_PRIOR_FUNCTION,
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let path = context.get_raw(0).as_str()?;
            Ok(RepoPath::from_git(path).map_or(1.0, |path| path_prior(&path)))
        },
    )
}

impl ChunkSearcher for SqliteStore {
    fn search(
        &self,
        query: &Query,
        mode: QueryMode,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<SearchOutcome, StorageError> {
        let matching = Matching::new(&match_expression(query, mode), filters);
        self.read_snapshot(|tx| {
            ensure_touched_sources_are_current(tx, filters)?;
            let counted = count_matches(tx, &matching)?;
            let hits = if counted == 0 || limit == 0 {
                Vec::new()
            } else {
                let ranked = rank_matches(tx, &matching, limit)?;
                load_hits(tx, &ranked)?
            };
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
                total_matches: counted.min(TOTAL_MATCHES_CAP),
                total_is_exact: counted <= TOTAL_MATCHES_CAP,
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

/// The `FROM ... WHERE` part shared by the ranking and the counting query, with its
/// parameters: the chunks that match the expression in a searchable source and pass
/// the source, language, and path filters.
struct Matching {
    from_where: String,
    params: Vec<Value>,
}

impl Matching {
    fn new(expression: &str, filters: &SearchFilters) -> Self {
        let mut from_where = String::from(
            "FROM chunks_fts \
             JOIN chunks c ON c.chunk_rowid = chunks_fts.rowid \
             JOIN sources s ON s.source_id = c.source_id \
             WHERE chunks_fts MATCH ? AND s.indexed_commit IS NOT NULL AND s.status <> 'removing'",
        );
        let mut params = vec![Value::Text(expression.to_owned())];
        push_in_list(
            &mut from_where,
            &mut params,
            "c.source_id",
            source_names(filters),
        );
        push_in_list(
            &mut from_where,
            &mut params,
            "c.language",
            filters
                .languages
                .iter()
                .map(|language| language.name().to_owned())
                .collect(),
        );
        if let Some(glob) = &filters.path {
            from_where.push_str(&format!(" AND {PATH_GLOB_FUNCTION}(?, c.path)"));
            params.push(Value::Text(glob.as_str().to_owned()));
        }
        Self { from_where, params }
    }

    /// Ranks the matches best first. Selects the row id and the score (negated bm25
    /// times the path prior, so documentation outranks tests at similar relevance
    /// and the prior acts before `LIMIT`); the sort keys are the columns of the
    /// tie-break (`ScoredChunk::ranking_order`). No text.
    fn ranked_sql(&self) -> String {
        let [path, heading, body] = COLUMN_WEIGHTS;
        format!(
            "SELECT c.chunk_rowid, \
             -bm25(chunks_fts, {path:?}, {heading:?}, {body:?}) * {PATH_PRIOR_FUNCTION}(c.path) AS score \
             {} \
             ORDER BY score DESC, c.source_id, c.path, c.start_line, c.chunk_id \
             LIMIT ?",
            self.from_where
        )
    }

    /// Counts the matches up to one past the cap, which is all that is needed to
    /// tell "exactly the cap" from "more than the cap".
    fn counting_sql(&self) -> String {
        format!(
            "SELECT COUNT(*) FROM (SELECT 1 {} LIMIT {})",
            self.from_where,
            TOTAL_MATCHES_CAP + 1
        )
    }
}

/// The number of matches, counted up to `TOTAL_MATCHES_CAP + 1`.
fn count_matches(connection: &Connection, matching: &Matching) -> Result<usize, StorageError> {
    let count: i64 = connection
        .prepare(&matching.counting_sql())
        .map_err(storage_error)?
        .query_row(params_from_iter(matching.params.iter()), |row| row.get(0))
        .map_err(storage_error)?;
    Ok(usize::try_from(count).unwrap_or(0))
}

/// Row id and score of the best `limit` matches, best first.
fn rank_matches(
    connection: &Connection,
    matching: &Matching,
    limit: usize,
) -> Result<Vec<(i64, f64)>, StorageError> {
    let mut params = matching.params.clone();
    params.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
    let mut statement = connection
        .prepare(&matching.ranked_sql())
        .map_err(storage_error)?;
    let rows = statement
        .query_map(params_from_iter(params), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
        })
        .map_err(storage_error)?;
    rows.map(|row| row.map_err(storage_error)).collect()
}

/// The stored chunks of the ranked winners, with their scores, in rank order.
fn load_hits(
    connection: &Connection,
    ranked: &[(i64, f64)],
) -> Result<Vec<ScoredChunk>, StorageError> {
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks WHERE chunk_rowid = ?1"
        ))
        .map_err(storage_error)?;
    let mut hits = Vec::with_capacity(ranked.len());
    for (rowid, score) in ranked {
        let chunk = statement
            .query_row([rowid], RawChunk::read)
            .map_err(storage_error)?
            .into_chunk()?;
        // `+ 0.0` turns a negated zero into a plain one.
        hits.push(ScoredChunk {
            chunk,
            score: score + 0.0,
        });
    }
    Ok(hits)
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

#[cfg(test)]
mod tests {
    use rag_core::domain::language::Language;
    use rag_core::domain::path_glob::PathGlob;
    use rag_core::domain::search_filters::SearchFilters;
    use rag_core::domain::source_id::SourceId;
    use rusqlite::Connection;

    use super::{register_path_glob, register_path_prior, Matching};
    use crate::sqlite::schema::initialize;

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        initialize(&mut connection).expect("schema");
        register_path_glob(&connection).expect("glob function");
        register_path_prior(&connection).expect("prior function");
        connection
    }

    fn everything() -> SearchFilters {
        SearchFilters {
            sources: vec![SourceId::parse("github.com/o/r-0123456789ab").expect("source id")],
            languages: vec![Language::parse("rust").expect("language")],
            path: Some(PathGlob::parse("src/**/*.rs").expect("glob")),
        }
    }

    fn plan(connection: &Connection, sql: &str, parameters: usize) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("explain");
        let params: Vec<rusqlite::types::Value> = (0..parameters)
            .map(|_| rusqlite::types::Value::Text("x".into()))
            .collect();
        let rows = statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                row.get::<_, String>(3)
            })
            .expect("plan rows");
        rows.map(|row| row.expect("plan row")).collect()
    }

    #[test]
    fn the_ranked_query_never_selects_or_sorts_by_the_chunk_text() {
        let matching = Matching::new("\"x\"", &everything());
        let sql = matching.ranked_sql();
        assert!(!sql.contains("text"), "{sql}");
        assert!(!sql.contains("heading_path"), "{sql}");
        assert!(sql.contains("ORDER BY") && sql.contains("LIMIT"), "{sql}");
        assert!(sql.contains("rag_path_glob(?, c.path)"), "{sql}");
        assert!(
            sql.contains("* rag_path_prior(c.path) AS score")
                && sql.contains("ORDER BY score DESC"),
            "the prior is part of the score the ORDER BY sorts, before LIMIT:\n{sql}"
        );
    }

    #[test]
    fn the_prior_function_applies_the_domain_multipliers_to_a_stored_path() {
        let connection = database();
        let prior = |path: &str| -> f64 {
            connection
                .query_row("SELECT rag_path_prior(?1)", [path], |row| row.get(0))
                .expect("prior call")
        };
        assert_eq!(prior("docs/guide.md"), 1.15);
        assert_eq!(prior("tests/a.rs"), 0.7);
        assert_eq!(prior("src/lib.rs"), 1.0);
        assert_eq!(prior("a/../b.rs"), 1.0, "an unsafe stored path is neutral");
    }

    #[test]
    fn the_plan_reaches_a_chunk_only_by_row_id_even_with_every_filter_set() {
        let connection = database();
        for filters in [SearchFilters::default(), everything()] {
            let matching = Matching::new("\"x\"", &filters);
            let parameters = matching.params.len() + 1;
            let plan = plan(&connection, &matching.ranked_sql(), parameters);
            let joined = plan.join("\n");
            assert!(
                joined.contains("SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
                "chunks are read by row id:\n{joined}"
            );
            let scanned: Vec<&str> = plan
                .iter()
                .filter_map(|line| line.strip_prefix("SCAN "))
                .filter_map(|rest| rest.split_whitespace().next())
                .collect();
            assert_eq!(
                scanned,
                ["chunks_fts"],
                "only the full-text index is scanned; chunks are never scanned, so the \
                 glob and the filters read no rows beyond the matches:\n{joined}"
            );
            assert!(
                joined.contains("VIRTUAL TABLE INDEX"),
                "the full-text index drives the query:\n{joined}"
            );
        }
    }

    #[test]
    fn the_counting_query_is_bounded_by_the_cap() {
        let sql = Matching::new("\"x\"", &everything()).counting_sql();
        assert!(sql.ends_with("LIMIT 1001)"), "{sql}");
        assert!(!sql.contains("ORDER BY") && !sql.contains("bm25"), "{sql}");
    }

    #[test]
    fn the_glob_function_applies_the_domain_rules_to_a_stored_path() {
        let connection = database();
        let check = |pattern: &str, path: &str| -> bool {
            connection
                .query_row("SELECT rag_path_glob(?1, ?2)", (pattern, path), |row| {
                    row.get(0)
                })
                .expect("glob call")
        };
        assert!(check("*.rs", "src/deep/lib.rs"), "no slash: the file name");
        assert!(check("src/**", "src/a/b.rs"));
        assert!(!check("docs/**", "src/a/b.rs"));
        assert!(
            !check("*.rs", "a/../b.rs"),
            "an unsafe stored path never matches"
        );
    }
}
