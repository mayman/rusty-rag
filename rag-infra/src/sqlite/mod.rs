//! The SQLite adapter: `SourceRepository`, `ChunkStore`, and `ChunkSearcher` over one
//! database file, `index.db`, in a directory chosen by the caller.
//!
//! # Opening: a read path and a write path
//!
//! `SqliteStore::open_for_read` is for commands that only look (search, show, list).
//! It opens one read-only connection and verifies the schema version and the FTS5
//! tokenizer configuration with plain `SELECT`s. It never takes the writer lock,
//! never runs DDL or a pragma that writes, and creates nothing, so it works while
//! another process holds `BEGIN IMMEDIATE` and in a directory it cannot write to.
//! The returned store refuses every write with `Failed`.
//!
//! `SqliteStore::open_for_write` is for commands that change the index. It creates
//! the directory and the file, switches to WAL, checks the schema version *before*
//! any DDL runs (a database of another version is refused untouched), creates the
//! schema when the database is new, and opens the read connection as well.
//!
//! # Connections and transactions
//!
//! The database runs in WAL mode with at most one write connection and one separate
//! read connection (each behind a mutex, since a `rusqlite::Connection` is not
//! `Sync`). Every write is a `BEGIN IMMEDIATE` transaction under a `busy_timeout`
//! (`DEFAULT_BUSY_TIMEOUT`, one minute, because a writer may legitimately queue
//! behind a long reindex): that is the single writer lock of the index. A writer that
//! cannot get it in time fails with `StorageError::Busy`. Readers keep a short wait
//! (`READER_BUSY_TIMEOUT`); in WAL mode they hardly ever wait. `commit_index` checks
//! `expected_base` inside its transaction, so of two commits from the same base
//! exactly one succeeds and the other gets `Conflict`. Searches never take the
//! writer lock: they run in one read transaction (one WAL snapshot) and return hits,
//! revisions, and the total from that snapshot.
//!
//! The write connection limits the WAL file after a checkpoint (`journal_size_limit`)
//! and, after a bulk write, runs a best-effort `wal_checkpoint(TRUNCATE)` with the
//! busy handler off: a reader that still holds a snapshot makes it give up at once
//! instead of stalling the writer, and the next write tries again. The read
//! connection has a 64 MiB page cache.
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
//! A test round-trips every token the splitter can emit through this tokenizer
//! (`fts5vocab`) and requires each to come back as exactly one, unchanged term.
//!
//! Queries are built from domain `Query` words (see `fts_query`), every term quoted,
//! so user text can never be parsed as FTS5 syntax.
//!
//! # Search cost
//!
//! A search ranks every match, so its cost does not depend on the limit. The ranked
//! query selects only the row id, the score, and the sort keys (no chunk text);
//! `ORDER BY ... LIMIT` keeps a bounded top-K; the text and heading of the winners
//! are fetched by row id afterwards. The match count is a separate counting subquery
//! that stops after `TOTAL_MATCHES_CAP + 1` rows.
//!
//! # Filters
//!
//! Source and language filters are SQL conditions. The path glob is a domain rule
//! (`PathGlob`), registered as the scalar function `rag_path_glob(pattern, path)`
//! that calls it, so it filters inside SQL before the limit and before counting.
//!
//! # Names and versions
//!
//! `name_key` (the domain's case-insensitive key) is computed in Rust and carries a
//! UNIQUE index, as does `chunk_id`. The tokenizer, chunker, and policy versions are
//! stored per source with its indexed revision, stamped inside `commit_index`; a
//! search that touches a source with other versions fails with `ReindexRequired`.

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
use rusqlite::{Connection, ErrorCode, OpenFlags, Transaction, TransactionBehavior};

use self::error::{failed, storage_error};

/// The database file inside the directory given to the `open_for_*` functions.
pub const DATABASE_FILE: &str = "index.db";

/// How long a writer waits for the writer lock before giving up with `Busy`.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the read connection waits on a lock. WAL readers rarely wait at all.
pub const READER_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Size the WAL file is cut back to after a checkpoint resets it.
const JOURNAL_SIZE_LIMIT_BYTES: i64 = 64 * 1024 * 1024;

/// Page cache of the read connection, in KiB (a negative `cache_size` is KiB).
const READER_CACHE_KIB: i64 = -64 * 1024;

/// The SQLite implementation of the catalog, the chunk store, and the searcher.
#[derive(Debug)]
pub struct SqliteStore {
    /// `None` for a store opened with `open_for_read`.
    writer: Option<Mutex<Connection>>,
    writer_busy_timeout: Duration,
    reader: Mutex<Connection>,
}

impl SqliteStore {
    /// Opens the database in `directory` for reading only. `IndexMissing` when the
    /// directory holds no initialised database; `Failed` when it has another schema
    /// version or tokenizer configuration. Writes through the returned store fail.
    pub fn open_for_read(directory: &Path) -> Result<Self, StorageError> {
        let file = directory.join(DATABASE_FILE);
        if !file.is_file() {
            return Err(StorageError::IndexMissing);
        }
        Ok(Self {
            writer: None,
            writer_busy_timeout: DEFAULT_BUSY_TIMEOUT,
            reader: Mutex::new(open_reader(&file)?),
        })
    }

    /// Opens the database in `directory` for reading and writing, creating the
    /// directory, the file, and the schema when they do not exist.
    pub fn open_for_write(directory: &Path) -> Result<Self, StorageError> {
        Self::open_for_write_with_busy_timeout(directory, DEFAULT_BUSY_TIMEOUT)
    }

    /// Like `open_for_write`, with an explicit writer-lock wait.
    pub fn open_for_write_with_busy_timeout(
        directory: &Path,
        busy_timeout: Duration,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(directory)
            .map_err(|e| failed(format!("cannot create the data directory: {e}")))?;
        let file = directory.join(DATABASE_FILE);

        let mut writer = Connection::open(&file).map_err(storage_error)?;
        writer.busy_timeout(busy_timeout).map_err(storage_error)?;
        enable_wal(&writer, busy_timeout)?;
        writer
            .execute_batch(&format!(
                "PRAGMA synchronous = NORMAL; PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT_BYTES};"
            ))
            .map_err(storage_error)?;
        schema::initialize(&mut writer)?;

        Ok(Self {
            writer: Some(Mutex::new(writer)),
            writer_busy_timeout: busy_timeout,
            reader: Mutex::new(open_reader(&file)?),
        })
    }

    /// The write connection, or `Failed` for a store opened with `open_for_read`.
    fn writer(&self) -> Result<&Mutex<Connection>, StorageError> {
        self.writer
            .as_ref()
            .ok_or_else(|| failed("the index was opened read-only"))
    }

    /// Runs `work` in a `BEGIN IMMEDIATE` transaction on the write connection and
    /// commits it when `work` succeeds. Any error rolls everything back.
    fn write<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let mut connection = lock(self.writer()?);
        run_in_transaction(&mut connection, work)
    }

    /// Like `write`, then truncates the WAL (best effort) because the write may
    /// have been large.
    fn write_bulk<T>(
        &self,
        work: impl FnOnce(&Transaction<'_>) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let mut connection = lock(self.writer()?);
        let value = run_in_transaction(&mut connection, work)?;
        self.truncate_wal(&connection);
        Ok(value)
    }

    /// `wal_checkpoint(TRUNCATE)` without waiting: a reader that still holds a
    /// snapshot keeps the WAL from being reset, and stalling the writer for it would
    /// be worse than leaving the log for the next write. The outcome is ignored: the
    /// data is committed either way.
    fn truncate_wal(&self, connection: &Connection) {
        let _ = connection.busy_timeout(Duration::ZERO);
        let _ = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            row.get::<_, i64>(0)
        });
        let _ = connection.busy_timeout(self.writer_busy_timeout);
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

fn run_in_transaction<T>(
    connection: &mut Connection,
    work: impl FnOnce(&Transaction<'_>) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    let value = work(&tx)?;
    tx.commit().map_err(storage_error)?;
    Ok(value)
}

/// A poisoned lock only means another thread panicked mid-operation; the connection
/// is still consistent because every write is a transaction.
fn lock(connection: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    connection.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Opens the read connection on `file` and verifies the schema with plain `SELECT`s.
/// Switches a database to WAL. When several processes do this to a brand-new file at
/// once, SQLite can answer `BUSY` straight away without consulting the busy handler,
/// so the switch is retried (it is idempotent) until `wait` has passed.
fn enable_wal(connection: &Connection, wait: Duration) -> Result<(), StorageError> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let switched = connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        });
        match switched {
            Ok(_) => return Ok(()),
            Err(error) => match storage_error(error) {
                StorageError::Busy if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => return Err(other),
            },
        }
    }
}

/// Opens the read connection.
///
/// A WAL database normally needs write access to its directory even to be read (the
/// shared-memory file). When the directory cannot be written and no write-ahead log
/// holds unflushed commits, the file is opened `immutable` instead: nothing can be
/// written by this process, and with an empty log the file alone is the whole truth.
fn open_reader(file: &Path) -> Result<Connection, StorageError> {
    let name = file
        .to_str()
        .ok_or_else(|| failed("the data directory path is not valid UTF-8"))?;
    let connection = connect_reader(name).map_err(storage_error)?;
    let probe = connection.query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    });
    let connection = match probe {
        Ok(_) => connection,
        Err(error)
            if error.sqlite_error_code() == Some(ErrorCode::CannotOpen)
                && write_ahead_log_is_empty(file) =>
        {
            drop(connection);
            connect_reader(&immutable_uri(name)).map_err(storage_error)?
        }
        Err(error) => return Err(storage_error(error)),
    };
    schema::verify(&connection)?;
    Ok(connection)
}

fn connect_reader(target: &str) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        target,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(READER_BUSY_TIMEOUT)?;
    connection.pragma_update(None, "cache_size", READER_CACHE_KIB)?;
    search::register_path_glob(&connection)?;
    Ok(connection)
}

fn write_ahead_log_is_empty(file: &Path) -> bool {
    let mut log = file.as_os_str().to_owned();
    log.push("-wal");
    match std::fs::metadata(log) {
        Ok(metadata) => metadata.len() == 0,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// `file:` URI of `path` with `immutable=1`, percent-encoding what a URI reserves.
fn immutable_uri(path: &str) -> String {
    let mut uri = String::from("file:");
    let slashed = path.replace('\\', "/");
    if !slashed.starts_with('/') {
        uri.push('/');
    }
    for c in slashed.chars() {
        match c {
            '%' | '?' | '#' | ' ' | '"' | '<' | '>' => {
                let mut buffer = [0; 4];
                for byte in c.encode_utf8(&mut buffer).bytes() {
                    uri.push_str(&format!("%{byte:02X}"));
                }
            }
            c => uri.push(c),
        }
    }
    uri.push_str("?immutable=1");
    uri
}

#[cfg(test)]
mod tests {
    use super::{immutable_uri, SqliteStore, DEFAULT_BUSY_TIMEOUT};

    fn pragma(store: &SqliteStore, reader: bool, name: &str) -> i64 {
        let connection = if reader {
            super::lock(&store.reader)
        } else {
            super::lock(store.writer.as_ref().expect("a write store"))
        };
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .expect("pragma")
    }

    fn write_store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        (SqliteStore::open_for_write(dir.path()).expect("open"), dir)
    }

    #[test]
    fn the_writer_waits_a_minute_for_the_lock_by_default() {
        let (store, _dir) = write_store();
        assert_eq!(DEFAULT_BUSY_TIMEOUT.as_secs(), 60);
        assert_eq!(pragma(&store, false, "busy_timeout"), 60_000);
    }

    #[test]
    fn the_reader_keeps_a_short_wait_whatever_the_writer_waits() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SqliteStore::open_for_write_with_busy_timeout(
            dir.path(),
            std::time::Duration::from_secs(90),
        )
        .expect("open");
        assert_eq!(pragma(&store, false, "busy_timeout"), 90_000);
        assert_eq!(pragma(&store, true, "busy_timeout"), 5_000);
    }

    #[test]
    fn the_reader_has_a_64_mib_cache() {
        let (store, _dir) = write_store();
        assert_eq!(pragma(&store, true, "cache_size"), -65_536);
    }

    #[test]
    fn the_write_ahead_log_is_size_limited() {
        let (store, _dir) = write_store();
        assert_eq!(
            pragma(&store, false, "journal_size_limit"),
            64 * 1024 * 1024
        );
        assert_eq!(pragma(&store, false, "synchronous"), 1, "NORMAL");
    }

    #[test]
    fn the_read_connection_cannot_write() {
        let (store, _dir) = write_store();
        let connection = super::lock(&store.reader);
        assert!(connection
            .execute("INSERT INTO meta VALUES ('x', 'y')", [])
            .is_err());
    }

    #[test]
    fn the_immutable_uri_escapes_what_a_uri_reserves() {
        assert_eq!(
            immutable_uri("C:\\Users\\me\\a b#c\\index.db"),
            "file:/C:/Users/me/a%20b%23c/index.db?immutable=1"
        );
        assert_eq!(
            immutable_uri("/home/me/100%/index.db"),
            "file:/home/me/100%25/index.db?immutable=1"
        );
    }
}
