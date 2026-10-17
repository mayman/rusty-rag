//! `ChunkStore` over the `files`, `chunks`, and `chunks_fts` tables. Every write is
//! one `BEGIN IMMEDIATE` transaction: validation happens before the first row is
//! touched, and any error rolls the transaction back, so a failed call changes
//! nothing.

use std::collections::{BTreeMap, BTreeSet};

use rag_core::application::error::StorageError;
use rag_core::application::ports::{ChunkStore, IndexChanges, IndexedFile, ReindexedChunk};
use rag_core::domain::chunk::Chunk;
use rag_core::domain::chunk_id::ChunkId;
use rag_core::domain::index_versions::IndexVersions;
use rag_core::domain::manifest::Manifest;
use rag_core::domain::oid::{BlobOid, CommitId};
use rag_core::domain::repo_path::RepoPath;
use rag_core::domain::source::Source;
use rag_core::domain::source_id::SourceId;
use rusqlite::{Connection, OptionalExtension};

use super::catalog::ensure_name_free;
use super::error::{failed, storage_error};
use super::records::{encode_heading_path, load_source, query_chunks};
use super::SqliteStore;

impl ChunkStore for SqliteStore {
    fn commit_index(
        &self,
        source: &Source,
        expected_base: Option<&CommitId>,
        changes: &IndexChanges,
    ) -> Result<(), StorageError> {
        self.write_bulk(|tx| {
            let stored = load_source(tx, source.id())?
                .ok_or_else(|| failed("the source is not registered"))?;
            let revision = source
                .indexed()
                .ok_or_else(|| failed("the source has no indexed revision to commit"))?;
            if stored.indexed().map(|r| &r.commit) != expected_base {
                return Err(StorageError::Conflict);
            }
            ensure_name_free(tx, source)?;
            ensure_current_tokenizer_and_policy(revision.versions)?;
            validate(source.id(), changes)?;
            if stored
                .indexed()
                .is_some_and(|old| old.versions != revision.versions)
            {
                ensure_every_stored_file_is_covered(tx, source.id(), changes)?;
            }

            // Every replaced file goes before any new one is inserted, so a chunk id
            // that moves from one replaced file to another is not a clash.
            for path in &changes.removals {
                delete_file(tx, source.id(), path)?;
            }
            for file in &changes.upserts {
                delete_file(tx, source.id(), &file.path)?;
            }
            for file in &changes.upserts {
                insert_file(tx, source.id(), file)?;
            }

            tx.execute(
                "UPDATE sources SET name = ?2, name_key = ?3, location = ?4, git_ref = ?5, \
                 status = ?6, indexed_commit = ?7, indexed_at = ?8, tokenizer_version = ?9, \
                 chunker_version = ?10, policy_version = ?11 WHERE source_id = ?1",
                (
                    source.id().as_str(),
                    source.name(),
                    source.name_key(),
                    source.location().as_str(),
                    source.git_ref().map(|r| r.as_str().to_owned()),
                    source.status().as_str(),
                    revision.commit.as_str(),
                    revision.at.epoch_seconds(),
                    revision.versions.tokenizer,
                    revision.versions.chunker,
                    revision.versions.policy,
                ),
            )
            .map_err(storage_error)?;
            Ok(())
        })
    }

    fn remove_source(&self, id: &SourceId) -> Result<(), StorageError> {
        self.write_bulk(|tx| {
            tx.execute(
                "DELETE FROM chunks_fts WHERE rowid IN \
                 (SELECT chunk_rowid FROM chunks WHERE source_id = ?1)",
                [id.as_str()],
            )
            .map_err(storage_error)?;
            for table in ["chunks", "files", "sources"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE source_id = ?1"),
                    [id.as_str()],
                )
                .map_err(storage_error)?;
            }
            Ok(())
        })
    }

    fn manifest(&self, source: &SourceId) -> Result<Manifest, StorageError> {
        self.read(|connection| {
            let mut statement = connection
                .prepare("SELECT path, blob_oid FROM files WHERE source_id = ?1")
                .map_err(storage_error)?;
            let rows = statement
                .query_map([source.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage_error)?;
            rows.into_iter()
                .map(|(path, blob)| {
                    let path = RepoPath::from_git(&path)
                        .map_err(|_| failed(format!("stored path {path:?} is invalid")))?;
                    let blob = BlobOid::parse(&blob)
                        .map_err(|_| failed(format!("stored blob id of {path} is invalid")))?;
                    Ok((path, blob))
                })
                .collect()
        })
    }

    fn chunk_count(&self, source: &SourceId) -> Result<usize, StorageError> {
        self.read(|connection| {
            let count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM chunks WHERE source_id = ?1",
                    [source.as_str()],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            usize::try_from(count).map_err(|_| failed("negative chunk count"))
        })
    }

    fn chunk(&self, id: &ChunkId) -> Result<Option<Chunk>, StorageError> {
        self.read(|connection| {
            Ok(
                query_chunks(connection, "WHERE chunk_id = ?1", [id.as_str()])?
                    .into_iter()
                    .next(),
            )
        })
    }

    fn chunks(&self, source: &SourceId) -> Result<Vec<Chunk>, StorageError> {
        self.read(|connection| query_chunks(connection, "WHERE source_id = ?1", [source.as_str()]))
    }

    fn stored_versions(&self, source: &SourceId) -> Result<Option<IndexVersions>, StorageError> {
        self.read(|connection| {
            Ok(load_source(connection, source)?
                .and_then(|stored| stored.indexed().map(|revision| revision.versions)))
        })
    }

    fn rewrite_index_fields(
        &self,
        source: &SourceId,
        fields: &[ReindexedChunk],
        versions: IndexVersions,
    ) -> Result<(), StorageError> {
        self.write_bulk(|tx| {
            let stored =
                load_source(tx, source)?.ok_or_else(|| failed("the source is not registered"))?;
            let revision = stored
                .indexed()
                .ok_or_else(|| failed("the source has no indexed revision"))?;
            if !versions.same_chunks_as(&revision.versions) {
                return Err(failed(
                    "a rewrite may only change the tokenizer version; re-chunk with commit_index",
                ));
            }
            ensure_current_tokenizer_and_policy(versions)?;

            let mut replacement = BTreeMap::new();
            for entry in fields {
                if replacement
                    .insert(entry.id.as_str(), &entry.fields)
                    .is_some()
                {
                    return Err(failed(format!("chunk {} is listed twice", entry.id)));
                }
            }
            let stored_rows = stored_chunk_rows(tx, source)?;
            let stored_ids: BTreeSet<&str> =
                stored_rows.iter().map(|(_, id)| id.as_str()).collect();
            if stored_ids != replacement.keys().copied().collect() {
                return Err(failed("fields must be given for exactly the stored chunks"));
            }

            let mut update = tx
                .prepare_cached(
                    "UPDATE chunks_fts SET path = ?2, heading = ?3, body = ?4 WHERE rowid = ?1",
                )
                .map_err(storage_error)?;
            for (rowid, id) in &stored_rows {
                let new = replacement[id.as_str()];
                update
                    .execute((rowid, &new.path, &new.heading, &new.body))
                    .map_err(storage_error)?;
            }
            drop(update);

            tx.execute(
                "UPDATE sources SET tokenizer_version = ?2, chunker_version = ?3, \
                 policy_version = ?4 WHERE source_id = ?1",
                (
                    source.as_str(),
                    versions.tokenizer,
                    versions.chunker,
                    versions.policy,
                ),
            )
            .map_err(storage_error)?;
            Ok(())
        })
    }
}

/// `(row id, chunk id)` of every stored chunk of the source.
fn stored_chunk_rows(
    connection: &Connection,
    source: &SourceId,
) -> Result<Vec<(i64, String)>, StorageError> {
    let mut statement = connection
        .prepare("SELECT chunk_rowid, chunk_id FROM chunks WHERE source_id = ?1")
        .map_err(storage_error)?;
    let rows = statement
        .query_map([source.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(storage_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage_error)?;
    Ok(rows)
}

/// The index is built by this build's domain, so it may not claim other tokenizer or
/// policy versions.
fn ensure_current_tokenizer_and_policy(versions: IndexVersions) -> Result<(), StorageError> {
    if versions.stamps_current_tokenizer_and_policy() {
        Ok(())
    } else {
        Err(failed(format!(
            "an index is stamped with the current tokenizer and policy versions only, \
             not {versions:?}"
        )))
    }
}

/// Rejects change sets that name a chunk of another source or path, upsert a path
/// twice, both upsert and remove a path, or carry the same chunk id twice. (A chunk
/// id that a file which stays also has is caught by the UNIQUE index.)
fn validate(source: &SourceId, changes: &IndexChanges) -> Result<(), StorageError> {
    let mut chunk_ids = BTreeSet::new();
    let mut upserted = BTreeSet::new();
    for file in &changes.upserts {
        if !upserted.insert(&file.path) {
            return Err(failed(format!("{} is upserted twice", file.path)));
        }
        if let Some(chunk) = file.chunks.iter().find(|c| !chunk_ids.insert(&c.id)) {
            return Err(failed(format!("chunk id {} is stored twice", chunk.id)));
        }
        if file
            .chunks
            .iter()
            .any(|chunk| &chunk.source != source || chunk.path != file.path)
        {
            return Err(failed(format!(
                "a chunk of {} belongs to another source or path",
                file.path
            )));
        }
    }
    if let Some(path) = changes.removals.iter().find(|p| upserted.contains(p)) {
        return Err(failed(format!("{path} is both removed and upserted")));
    }
    Ok(())
}

/// A version change replaces the whole index: every stored path must be upserted or
/// removed, or chunks of two versions would live side by side.
fn ensure_every_stored_file_is_covered(
    connection: &Connection,
    source: &SourceId,
    changes: &IndexChanges,
) -> Result<(), StorageError> {
    let covered: BTreeSet<&str> = changes
        .upserts
        .iter()
        .map(|file| file.path.as_str())
        .chain(changes.removals.iter().map(RepoPath::as_str))
        .collect();
    let mut statement = connection
        .prepare("SELECT path FROM files WHERE source_id = ?1")
        .map_err(storage_error)?;
    let stored = statement
        .query_map([source.as_str()], |row| row.get::<_, String>(0))
        .map_err(storage_error)?;
    for path in stored {
        let path = path.map_err(storage_error)?;
        if !covered.contains(path.as_str()) {
            return Err(failed(format!(
                "a version change must rewrite every stored file, but {path} is not covered"
            )));
        }
    }
    Ok(())
}

/// Drops one stored file with its chunks and full-text rows. No-op when absent.
fn delete_file(
    connection: &Connection,
    source: &SourceId,
    path: &RepoPath,
) -> Result<(), StorageError> {
    let file_id: Option<i64> = connection
        .query_row(
            "SELECT file_id FROM files WHERE source_id = ?1 AND path = ?2",
            (source.as_str(), path.as_str()),
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    let Some(file_id) = file_id else {
        return Ok(());
    };
    connection
        .execute(
            "DELETE FROM chunks_fts WHERE rowid IN \
             (SELECT chunk_rowid FROM chunks WHERE file_id = ?1)",
            [file_id],
        )
        .map_err(storage_error)?;
    connection
        .execute("DELETE FROM chunks WHERE file_id = ?1", [file_id])
        .map_err(storage_error)?;
    connection
        .execute("DELETE FROM files WHERE file_id = ?1", [file_id])
        .map_err(storage_error)?;
    Ok(())
}

fn line_number(line: usize) -> Result<i64, StorageError> {
    i64::try_from(line).map_err(|_| failed("line number out of range"))
}

/// Stores a file, its chunks, and the full-text fields the chunks carry.
fn insert_file(
    connection: &Connection,
    source: &SourceId,
    file: &IndexedFile,
) -> Result<(), StorageError> {
    let size = i64::try_from(file.size).map_err(|_| failed("file size out of range"))?;
    connection
        .execute(
            "INSERT INTO files (source_id, path, blob_oid, size) VALUES (?1, ?2, ?3, ?4)",
            (
                source.as_str(),
                file.path.as_str(),
                file.blob.as_str(),
                size,
            ),
        )
        .map_err(storage_error)?;
    let file_id = connection.last_insert_rowid();

    let mut insert_chunk = connection
        .prepare_cached(
            "INSERT INTO chunks (chunk_id, source_id, file_id, path, language, start_line, \
             end_line, heading_path, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .map_err(storage_error)?;
    let mut insert_fields = connection
        .prepare_cached(
            "INSERT INTO chunks_fts (rowid, path, heading, body) VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(storage_error)?;
    for chunk in &file.chunks {
        insert_chunk
            .execute((
                chunk.id.as_str(),
                source.as_str(),
                file_id,
                chunk.path.as_str(),
                chunk.language.name(),
                line_number(chunk.lines.start())?,
                line_number(chunk.lines.end())?,
                encode_heading_path(&chunk.heading_path)?,
                &chunk.text,
            ))
            .map_err(storage_error)?;
        let rowid = connection.last_insert_rowid();
        let fields = chunk.index_fields();
        insert_fields
            .execute((rowid, fields.path, fields.heading, fields.body))
            .map_err(storage_error)?;
    }
    Ok(())
}
