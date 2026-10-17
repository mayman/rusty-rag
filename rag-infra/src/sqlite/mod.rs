//! The SQLite adapter: `SourceRepository`, `ChunkStore`, and `ChunkSearcher` over one
//! database file, `index.db`, in a directory chosen by the caller.
//!
//! # Connections and transactions
//!
//! The database runs in WAL mode with one write connection and one separate read
//! connection (each behind a mutex, since a `rusqlite::Connection` is not `Sync`).
//! Every write is a `BEGIN IMMEDIATE` transaction under a `busy_timeout`: that is
//! the single writer lock of the index. A writer that cannot get it in time fails
//! with `StorageError::Busy`. `commit_index` checks `expected_base` inside that
//! transaction, so of two commits from the same base exactly one succeeds and the
//! other gets `Conflict`. Searches never take the writer lock: they run in one
//! read transaction (one WAL snapshot) and return hits, revisions, and the total
//! from that snapshot.
//!
//! # Tokenizer decision
//!
//! `chunks_fts` is a self-contained FTS5 table with the columns `path`, `heading`,
//! `body`, holding the already tokenized `IndexFields` that the domain computed
//! (`IdentifierSplitter::index_tokens`: lowercase alphanumeric tokens separated by
//! spaces). The tokenizer is `unicode61 remove_diacritics 0 categories 'L* N* Co M*'`:
//!
//! - No `tokenchars`. The splitter owns every separator (`_ - . /`) and removes it
//!   before indexing, emitting the joined form (`parseconfig`) next to the parts
//!   (`parse`, `config`). Making `_` a token character would only matter for text
//!   that bypasses the splitter, and would then glue `parse_config` into one token
//!   that `parseConfig` could not reach. With the default separators `parse_config`
//!   and `parseConfig` reach the same chunk through the joined token or through the
//!   parts, on both the index and the query side.
//! - `remove_diacritics 0`: accents are significant, so `naive` does not find
//!   `naïve`. (The FTS5 default would fold them.)
//! - `M*` is added to the default token categories (`L* N* Co`) so that alphabetic
//!   combining marks, which the splitter keeps inside a word, do not split it into
//!   several tokens (for example Devanagari vowel signs).
//!
//! Queries are built from domain `Query` words (see `fts_query`), every term quoted,
//! so user text can never be parsed as FTS5 syntax.
//!
//! # Filters
//!
//! Source and language filters are SQL conditions. The path glob is a domain rule
//! (`PathGlob`) and is applied row by row, in ranking order, before the limit is
//! taken and while counting `total_matches`. Registering it as an SQL scalar
//! function needs rusqlite's `functions` feature, which this crate does not enable;
//! the observable behavior is the same.
//!
//! # Names and versions
//!
//! `name_key` (the domain's case-insensitive key) is computed in Rust and carries a
//! UNIQUE index. The tokenizer, chunker, and policy versions are stored per source
//! with its indexed revision, stamped inside `commit_index`; a search that touches
//! a source with other versions fails with `ReindexRequired`.

mod catalog;
mod error;
mod fts_query;
mod index;
mod records;
mod schema;
mod search;

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rag_core::application::error::StorageError;
use rusqlite::{Connection, Transaction, TransactionBehavior};

use self::error::{failed, storage_error};

/// The database file inside the directory given to `SqliteStore::open`.
pub const DATABASE_FILE: &str = "index.db";

/// How long a writer waits for the writer lock before giving up with `Busy`.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The SQLite implementation of the catalog, the chunk store, and the searcher.
#[derive(Debug)]
pub struct SqliteStore {
    writer: Mutex<Connection>,
    reader: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens the database in `directory`, creating the directory, the file, and the
    /// schema when they do not exist.
    pub fn open(directory: &Path) -> Result<Self, StorageError> {
        Self::open_with_busy_timeout(directory, DEFAULT_BUSY_TIMEOUT)
    }

    /// Like `open`, with an explicit writer-lock wait.
    pub fn open_with_busy_timeout(
        directory: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(directory)
            .map_err(|e| failed(format!("cannot create the data directory: {e}")))?;
        let file = directory.join(DATABASE_FILE);

        let mut writer = Connection::open(&file).map_err(storage_error)?;
        writer.busy_timeout(busy_timeout).map_err(storage_error)?;
        writer
            .query_row("PRAGMA journal_mode = WAL", [], |row| {
                row.get::<_, String>(0)
            })
            .map_err(storage_error)?;
        writer
            .execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(storage_error)?;
        schema::initialize(&mut writer)?;

        let reader = Connection::open(&file).map_err(storage_error)?;
        reader.busy_timeout(busy_timeout).map_err(storage_error)?;
        reader
            .execute_batch("PRAGMA query_only = ON;")
            .map_err(storage_error)?;

        Ok(Self {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
        })
    }

    /// Opens an existing database; `IndexMissing` when `directory` holds none.
    pub fn open_existing(directory: &Path) -> Result<Self, StorageError> {
        if !directory.join(DATABASE_FILE).is_file() {
            return Err(StorageError::IndexMissing);
        }
        Self::open(directory)
    }

    /// Runs `work` in a `BEGIN IMMEDIATE` transaction on the write connection and
    /// commits it when `work` succeeds. Any error rolls everything back.
    fn write<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let mut connection = lock(&self.writer);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let value = work(&tx)?;
        tx.commit().map_err(storage_error)?;
        Ok(value)
    }

    /// Runs `work` on the read connection (each statement sees committed data).
    fn read<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        work(&lock(&self.reader))
    }

    /// Runs `work` in one read transaction (a single snapshot of the database).
    fn read_snapshot<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let mut connection = lock(&self.reader);
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(storage_error)?;
        work(&tx)
    }
}

/// A poisoned lock only means another thread panicked mid-operation; the connection
/// is still consistent because every write is a transaction.
fn lock(connection: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    connection.lock().unwrap_or_else(PoisonError::into_inner)
}
