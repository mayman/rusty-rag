//! `SourceRepository` over the `sources` table.

use rag_core::application::error::StorageError;
use rag_core::application::ports::SourceRepository;
use rag_core::domain::source::{name_key, Source, SourceStatus};
use rag_core::domain::source_id::SourceId;
use rusqlite::{Connection, OptionalExtension};

use super::error::{failed, storage_error};
use super::records::{load_source, query_sources, RawSource, SOURCE_COLUMNS};
use super::SqliteStore;

/// Fails with `NameTaken` when a source other than `source` has its name key.
pub(super) fn ensure_name_free(
    connection: &Connection,
    source: &Source,
) -> Result<(), StorageError> {
    let taken: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sources WHERE name_key = ?1 AND source_id <> ?2",
            (source.name_key(), source.id().as_str()),
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match taken {
        Some(_) => Err(StorageError::NameTaken(source.name().into())),
        None => Ok(()),
    }
}

impl SourceRepository for SqliteStore {
    fn get(&self, id: &SourceId) -> Result<Option<Source>, StorageError> {
        self.read(|connection| load_source(connection, id))
    }

    fn find_by_name(&self, name: &str) -> Result<Option<Source>, StorageError> {
        self.read(|connection| {
            connection
                .query_row(
                    &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE name_key = ?1"),
                    [name_key(name)],
                    RawSource::read,
                )
                .optional()
                .map_err(storage_error)?
                .map(RawSource::into_source)
                .transpose()
        })
    }

    fn list(&self) -> Result<Vec<Source>, StorageError> {
        self.read(|connection| query_sources(connection, "", []))
    }

    fn save(&self, source: &Source) -> Result<(), StorageError> {
        self.write(|tx| {
            ensure_name_free(tx, source)?;
            let stored = load_source(tx, source.id())?;
            let has_revision = stored.as_ref().is_some_and(|s| s.indexed().is_some());
            if source.status() == SourceStatus::Ready && !has_revision {
                return Err(failed(
                    "a ready source needs an indexed revision; commit an index first",
                ));
            }
            let git_ref = source.git_ref().map(|r| r.as_str().to_owned());
            if stored.is_some() {
                tx.execute(
                    "UPDATE sources SET name = ?2, name_key = ?3, location = ?4, \
                     git_ref = ?5, status = ?6 WHERE source_id = ?1",
                    (
                        source.id().as_str(),
                        source.name(),
                        source.name_key(),
                        source.location().as_str(),
                        git_ref,
                        source.status().as_str(),
                    ),
                )
            } else {
                tx.execute(
                    "INSERT INTO sources (source_id, name, name_key, location, git_ref, status) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    (
                        source.id().as_str(),
                        source.name(),
                        source.name_key(),
                        source.location().as_str(),
                        git_ref,
                        source.status().as_str(),
                    ),
                )
            }
            .map_err(storage_error)?;
            Ok(())
        })
    }
}
