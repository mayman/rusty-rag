//! The database schema, the FTS5 tokenizer configuration, and the pinned bm25
//! column weights. See the module documentation of `sqlite` for why the tokenizer
//! is configured the way it is.

use rag_core::application::error::StorageError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::error::{failed, storage_error};

/// Version of this layout. A database stamped with another version is refused.
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The FTS5 `tokenize` option. `unicode61` with no extra `tokenchars` (the domain
/// `IdentifierSplitter` already removed every separator from the indexed text),
/// `remove_diacritics 0` (accents are not folded), and `M*` added to the token
/// categories so alphabetic combining marks stay inside their word, as they do in
/// the splitter.
pub(crate) const FTS_TOKENIZER: &str = "unicode61 remove_diacritics 0 categories 'L* N* Co M*'";

/// bm25 weights of the `chunks_fts` columns `path`, `heading`, `body`. A term in a
/// file path counts more than one in a heading, which counts more than one in the
/// body. Pinned here and covered by a test; the order is the column order of the
/// FTS5 table.
pub(crate) const COLUMN_WEIGHTS: [f64; 3] = [3.0, 2.0, 1.0];

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS sources (
    source_id         TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    name_key          TEXT NOT NULL,
    location          TEXT NOT NULL,
    git_ref           TEXT,
    status            TEXT NOT NULL,
    indexed_commit    TEXT,
    indexed_at        INTEGER,
    tokenizer_version INTEGER,
    chunker_version   INTEGER,
    policy_version    INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS sources_name_key ON sources (name_key);

CREATE TABLE IF NOT EXISTS files (
    file_id   INTEGER PRIMARY KEY,
    source_id TEXT NOT NULL,
    path      TEXT NOT NULL,
    blob_oid  TEXT NOT NULL,
    size      INTEGER NOT NULL,
    UNIQUE (source_id, path)
);

CREATE TABLE IF NOT EXISTS chunks (
    chunk_rowid  INTEGER PRIMARY KEY,
    chunk_id     TEXT NOT NULL,
    source_id    TEXT NOT NULL,
    file_id      INTEGER NOT NULL,
    path         TEXT NOT NULL,
    language     TEXT NOT NULL,
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    heading_path TEXT NOT NULL,
    text         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_by_id ON chunks (chunk_id);
CREATE INDEX IF NOT EXISTS chunks_by_file ON chunks (file_id);
CREATE INDEX IF NOT EXISTS chunks_by_source ON chunks (source_id, path, start_line);
";

/// Creates the schema when the database is new and refuses one with another schema
/// version. Runs in one `BEGIN IMMEDIATE` transaction, so two processes opening a
/// fresh database at once cannot both initialise it.
pub(crate) fn initialize(connection: &mut Connection) -> Result<(), StorageError> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    tx.execute_batch(TABLES).map_err(storage_error)?;
    tx.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(path, heading, body, tokenize = \"{FTS_TOKENIZER}\");"
    ))
    .map_err(storage_error)?;
    let stored: Option<String> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    match stored {
        None => {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('fts_tokenizer', ?2)",
                (SCHEMA_VERSION.to_string(), FTS_TOKENIZER),
            )
            .map_err(storage_error)?;
        }
        Some(found) if found == SCHEMA_VERSION.to_string() => {}
        Some(found) => {
            return Err(failed(format!(
                "the database has schema version {found}, this build expects {SCHEMA_VERSION}"
            )));
        }
    }
    tx.commit().map_err(storage_error)
}

#[cfg(test)]
mod tests {
    use super::{initialize, FTS_TOKENIZER};
    use rusqlite::Connection;

    fn fts_with(rows: &[&str]) -> Connection {
        let mut connection = Connection::open_in_memory().expect("in-memory database");
        initialize(&mut connection).expect("schema");
        for (i, body) in rows.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO chunks_fts (rowid, path, heading, body) VALUES (?1, '', '', ?2)",
                    (i as i64 + 1, body),
                )
                .expect("insert");
        }
        connection
    }

    fn matches(connection: &Connection, expression: &str) -> Vec<i64> {
        let mut statement = connection
            .prepare("SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY rowid")
            .expect("prepare");
        let rows = statement
            .query_map([expression], |row| row.get(0))
            .expect("query");
        rows.map(|r| r.expect("row")).collect()
    }

    #[test]
    fn the_tokenizer_option_is_accepted_by_the_bundled_sqlite() {
        let connection = fts_with(&["alpha"]);
        assert_eq!(matches(&connection, "\"alpha\""), [1]);
        assert!(FTS_TOKENIZER.contains("remove_diacritics 0"));
    }

    #[test]
    fn split_identifier_tokens_are_found_by_joined_form_and_by_parts() {
        let connection = fts_with(&["parseconfig parse config"]);
        assert_eq!(matches(&connection, "\"parseconfig\""), [1]);
        assert_eq!(matches(&connection, "\"parse\" AND \"config\""), [1]);
    }

    #[test]
    fn diacritics_are_not_folded() {
        let connection = fts_with(&["na\u{ef}ve"]);
        assert_eq!(matches(&connection, "\"na\u{ef}ve\""), [1]);
        assert!(matches(&connection, "\"naive\"").is_empty());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let connection = fts_with(&["alpha"]);
        assert_eq!(matches(&connection, "\"ALPHA\""), [1]);
    }

    #[test]
    fn digits_and_non_latin_letters_are_token_characters() {
        let connection = fts_with(&["utf8 \u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}"]);
        assert_eq!(matches(&connection, "\"utf8\""), [1]);
        assert_eq!(
            matches(
                &connection,
                "\"\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}\""
            ),
            [1]
        );
    }

    #[test]
    fn an_alphabetic_combining_mark_stays_inside_its_word() {
        // Devanagari "hindi" ends in a vowel sign (category Mc, Alphabetic).
        let word = "\u{939}\u{93f}\u{928}\u{94d}\u{926}\u{940}";
        let connection = fts_with(&[word]);
        assert_eq!(matches(&connection, &format!("\"{word}\"")), [1]);
        assert!(matches(&connection, "\"\u{939}\"").is_empty());
    }

    #[test]
    fn an_underscore_is_a_token_break_not_a_token_character() {
        // The splitter owns separators and the index never contains `_`; were it a
        // token character, a stray `parse_config` would be one token.
        let connection = fts_with(&["parse_config"]);
        assert_eq!(matches(&connection, "\"parse\""), [1]);
        assert_eq!(matches(&connection, "\"config\""), [1]);
    }

    #[test]
    fn a_database_with_another_schema_version_is_refused() {
        let mut connection = fts_with(&[]);
        connection
            .execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .expect("update");
        assert!(initialize(&mut connection).is_err());
    }

    #[test]
    fn initialising_twice_keeps_the_schema() {
        let mut connection = fts_with(&["alpha"]);
        initialize(&mut connection).expect("second initialisation");
        assert_eq!(matches(&connection, "\"alpha\""), [1]);
    }
}
