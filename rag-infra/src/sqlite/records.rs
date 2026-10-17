//! Rows of the `sources` and `chunks` tables and their translation to and from
//! domain values. Stored values are restored leniently (`RepoPath::from_git`,
//! `GitLocation::restore`); a row that no longer parses is a `Failed` error rather
//! than a panic.

use rag_core::application::error::StorageError;
use rag_core::domain::chunk::Chunk;
use rag_core::domain::chunk_id::ChunkId;
use rag_core::domain::git_location::GitLocation;
use rag_core::domain::git_ref::GitRef;
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::indexed_at::IndexedAt;
use rag_core::domain::language::Language;
use rag_core::domain::line_range::LineRange;
use rag_core::domain::oid::CommitId;
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::source::{IndexedRevision, LastError, Source, SourceStatus};
use rag_core::domain::source_id::SourceId;
use rusqlite::{Connection, OptionalExtension, Params, Row};

use super::error::{failed, storage_error};

/// Select list matching `RawSource::read`.
pub(crate) const SOURCE_COLUMNS: &str = "source_id, name, location, git_ref, status, \
     indexed_commit, indexed_at, tokenizer_version, chunker_version, policy_version, \
     last_error_code, last_error_message, last_error_at";

/// Select list matching `RawChunk::read`.
pub(crate) const CHUNK_COLUMNS: &str =
    "chunk_id, source_id, path, language, start_line, end_line, heading_path, text";

/// A `sources` row as stored, before it is checked against the domain.
pub(crate) struct RawSource {
    id: String,
    name: String,
    location: String,
    git_ref: Option<String>,
    status: String,
    commit: Option<String>,
    at: Option<i64>,
    versions: [Option<i64>; 3],
    last_error: Option<(String, String, i64)>,
}

impl RawSource {
    pub(crate) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            location: row.get(2)?,
            git_ref: row.get(3)?,
            status: row.get(4)?,
            commit: row.get(5)?,
            at: row.get(6)?,
            versions: [row.get(7)?, row.get(8)?, row.get(9)?],
            last_error: match (row.get(10)?, row.get(11)?, row.get(12)?) {
                (Some(code), Some(message), Some(at)) => Some((code, message, at)),
                _ => None,
            },
        })
    }

    pub(crate) fn into_source(self) -> Result<Source, StorageError> {
        let corrupt =
            |what: &str| failed(format!("stored source {} has an invalid {what}", self.id));
        let id = SourceId::parse(&self.id).map_err(|_| corrupt("id"))?;
        let location = GitLocation::restore(&self.location).map_err(|_| corrupt("location"))?;
        let git_ref = self
            .git_ref
            .as_deref()
            .map(GitRef::parse)
            .transpose()
            .map_err(|_| corrupt("ref"))?;
        let status = SourceStatus::parse(&self.status).ok_or_else(|| corrupt("status"))?;
        let indexed = match (&self.commit, self.at, self.versions) {
            (Some(commit), Some(at), [Some(tokenizer), Some(chunker), Some(policy)]) => {
                Some(IndexedRevision {
                    commit: CommitId::parse(commit).map_err(|_| corrupt("commit"))?,
                    at: IndexedAt::from_epoch_seconds(at),
                    versions: IndexVersions {
                        tokenizer: version(tokenizer).ok_or_else(|| corrupt("version"))?,
                        chunker: version(chunker).ok_or_else(|| corrupt("version"))?,
                        policy: version(policy).ok_or_else(|| corrupt("version"))?,
                    },
                })
            }
            (None, _, _) => None,
            _ => return Err(corrupt("revision")),
        };
        let last_error = self.last_error.map(|(code, message, at)| LastError {
            code,
            message,
            at: IndexedAt::from_epoch_seconds(at),
        });
        Source::restore(id, self.name.clone(), location, git_ref, status, indexed)
            .map(|source| source.with_last_error(last_error))
            .map_err(|e| failed(format!("stored source {}: {e}", self.id)))
    }
}

fn version(stored: i64) -> Option<u32> {
    u32::try_from(stored).ok()
}

/// A `chunks` row as stored.
pub(crate) struct RawChunk {
    id: String,
    source: String,
    path: String,
    language: String,
    start: i64,
    end: i64,
    heading_path: String,
    text: String,
}

impl RawChunk {
    pub(crate) fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            source: row.get(1)?,
            path: row.get(2)?,
            language: row.get(3)?,
            start: row.get(4)?,
            end: row.get(5)?,
            heading_path: row.get(6)?,
            text: row.get(7)?,
        })
    }

    pub(crate) fn into_chunk(self) -> Result<Chunk, StorageError> {
        let corrupt =
            |what: &str| failed(format!("stored chunk {} has an invalid {what}", self.id));
        let lines = usize::try_from(self.start)
            .ok()
            .zip(usize::try_from(self.end).ok())
            .and_then(|(start, end)| LineRange::new(start, end))
            .ok_or_else(|| corrupt("line range"))?;
        Ok(Chunk {
            id: ChunkId::parse(&self.id).ok_or_else(|| corrupt("id"))?,
            source: SourceId::parse(&self.source).map_err(|_| corrupt("source"))?,
            path: RepoPath::from_git(&self.path).map_err(|_| corrupt("path"))?,
            lines,
            heading_path: serde_json::from_str(&self.heading_path)
                .map_err(|_| corrupt("heading path"))?,
            language: Language::parse(&self.language).ok_or_else(|| corrupt("language"))?,
            text: self.text,
        })
    }
}

/// The heading path as stored: a JSON list, never joined text.
pub(crate) fn encode_heading_path(heading_path: &[String]) -> Result<String, StorageError> {
    serde_json::to_string(heading_path).map_err(|e| failed(format!("heading path: {e}")))
}

/// The stored source with this id.
pub(crate) fn load_source(
    connection: &Connection,
    id: &SourceId,
) -> Result<Option<Source>, StorageError> {
    connection
        .query_row(
            &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE source_id = ?1"),
            [id.as_str()],
            RawSource::read,
        )
        .optional()
        .map_err(storage_error)?
        .map(RawSource::into_source)
        .transpose()
}

/// Every source matching `where_clause` (which may be empty), ordered by id.
pub(crate) fn query_sources(
    connection: &Connection,
    where_clause: &str,
    params: impl Params,
) -> Result<Vec<Source>, StorageError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {SOURCE_COLUMNS} FROM sources {where_clause} ORDER BY source_id"
        ))
        .map_err(storage_error)?;
    let raws = statement
        .query_map(params, RawSource::read)
        .map_err(storage_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?;
    raws.into_iter().map(RawSource::into_source).collect()
}

/// Every chunk matching `where_clause`, in document order (path, first line, row).
pub(crate) fn query_chunks(
    connection: &Connection,
    where_clause: &str,
    params: impl Params,
) -> Result<Vec<Chunk>, StorageError> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT {CHUNK_COLUMNS} FROM chunks {where_clause} \
             ORDER BY path, start_line, chunk_rowid"
        ))
        .map_err(storage_error)?;
    let raws = statement
        .query_map(params, RawChunk::read)
        .map_err(storage_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?;
    raws.into_iter().map(RawChunk::into_chunk).collect()
}
