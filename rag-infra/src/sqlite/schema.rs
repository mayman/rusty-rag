//! The database schema, the FTS5 tokenizer configuration, and the pinned bm25
//! column weights. See the module documentation of `sqlite` for why the tokenizer
//! is configured the way it is.

use rag_core::application::error::StorageError;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::error::{failed, storage_error};

/// Version of this layout. A database stamped with another version is refused.
///
/// 2: `chunk_id` carries a UNIQUE index.
/// 3: `sources` keeps the last error (`last_error_code`, `last_error_message`,
/// `last_error_at`).
pub(crate) const SCHEMA_VERSION: u32 = 3;

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
    policy_version    INTEGER,
    last_error_code    TEXT,
    last_error_message TEXT,
    last_error_at      INTEGER
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
CREATE UNIQUE INDEX IF NOT EXISTS chunks_by_id ON chunks (chunk_id);
CREATE INDEX IF NOT EXISTS chunks_by_file ON chunks (file_id);
CREATE INDEX IF NOT EXISTS chunks_by_source ON chunks (source_id, path, start_line);
";

/// Creates the schema when the database is new and refuses one with another schema
/// version or tokenizer configuration, before any DDL touches it.
///
/// The common case (an initialised database) is decided by plain `SELECT`s and takes
/// no lock. A new database is initialised in one `BEGIN IMMEDIATE` transaction that
/// looks again, so two processes opening a fresh database at once cannot both
/// initialise it.
pub(crate) fn initialize(connection: &mut Connection) -> Result<(), StorageError> {
    if has_meta(connection)? {
        return check_meta(connection);
    }
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage_error)?;
    if has_meta(&tx)? {
        return check_meta(&tx);
    }
    tx.execute_batch(TABLES).map_err(storage_error)?;
    tx.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(path, heading, body, tokenize = \"{FTS_TOKENIZER}\");"
    ))
    .map_err(storage_error)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1), ('fts_tokenizer', ?2)",
        (SCHEMA_VERSION.to_string(), FTS_TOKENIZER),
    )
    .map_err(storage_error)?;
    tx.commit().map_err(storage_error)
}

/// Checks an existing database without changing it: `IndexMissing` when it has not
/// been initialised, `Failed` when its schema version or tokenizer configuration is
/// not the one this build uses.
pub(crate) fn verify(connection: &Connection) -> Result<(), StorageError> {
    if !has_meta(connection)? {
        return Err(StorageError::IndexMissing);
    }
    check_meta(connection)
}

fn has_meta(connection: &Connection) -> Result<bool, StorageError> {
    connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get(0),
        )
        .map_err(storage_error)
}

fn meta_value(connection: &Connection, key: &str) -> Result<Option<String>, StorageError> {
    connection
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(storage_error)
}

fn check_meta(connection: &Connection) -> Result<(), StorageError> {
    match meta_value(connection, "schema_version")? {
        Some(found) if found == SCHEMA_VERSION.to_string() => {}
        Some(found) => {
            return Err(failed(format!(
                "the database has schema version {found}, this build expects {SCHEMA_VERSION}"
            )));
        }
        None => return Err(failed("the database has no schema version")),
    }
    match meta_value(connection, "fts_tokenizer")? {
        Some(found) if found == FTS_TOKENIZER => Ok(()),
        Some(found) => Err(failed(format!(
            "the database was built with the FTS5 tokenizer {found:?}, this build uses {FTS_TOKENIZER:?}"
        ))),
        None => Err(failed("the database has no tokenizer configuration")),
    }
}
#[cfg(test)]
mod tests {
    use super::{initialize, FTS_TOKENIZER};
    use rag_core::domain::identifier_splitter::IdentifierSplitter;
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

    /// Every token the splitter can emit, over all Unicode scalar values: the text
    /// `x<c>x` for each `c`, the tokens of each such text. Together they cover every
    /// character category the splitter keeps inside a word.
    fn every_token_the_splitter_can_emit() -> Vec<String> {
        let mut tokens = std::collections::BTreeSet::new();
        for c in (0..=char::MAX as u32).filter_map(char::from_u32) {
            for text in [format!("x{c}x"), format!("{c}"), format!("X{c}X")] {
                tokens.extend(IdentifierSplitter::index_tokens(&text));
            }
        }
        tokens.into_iter().collect()
    }

    #[test]
    fn every_token_the_splitter_emits_survives_the_fts5_tokenizer_unchanged() {
        let tokens = every_token_the_splitter_can_emit();
        assert!(tokens.len() > 100_000, "fixture: {} tokens", tokens.len());
        let connection = fts_with(&[]);
        connection
            .execute_batch("CREATE VIRTUAL TABLE vocab USING fts5vocab(chunks_fts, 'instance')")
            .expect("fts5vocab");
        let tx = connection.unchecked_transaction().expect("transaction");
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO chunks_fts (rowid, path, heading, body) VALUES (?1, '', '', ?2)",
                )
                .expect("prepare");
            for (i, token) in tokens.iter().enumerate() {
                insert.execute((i as i64 + 1, token)).expect("insert");
            }
        }
        tx.commit().expect("commit");

        let mut terms: std::collections::BTreeMap<i64, Vec<String>> =
            std::collections::BTreeMap::new();
        let mut statement = connection
            .prepare("SELECT doc, term FROM vocab ORDER BY doc, \"offset\"")
            .expect("prepare vocab");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .expect("vocab rows");
        for row in rows {
            let (doc, term) = row.expect("row");
            terms.entry(doc).or_default().push(term);
        }

        let changed: Vec<String> = tokens
            .iter()
            .enumerate()
            .filter_map(|(i, token)| {
                let found = terms.get(&(i as i64 + 1));
                let unchanged = found.is_some_and(|found| found.len() == 1 && found[0] == *token);
                (!unchanged).then(|| format!("{token:?} became {found:?}"))
            })
            .collect();
        assert!(
            changed.is_empty(),
            "{} of {} tokens change in the FTS5 tokenizer, e.g. {:#?}",
            changed.len(),
            tokens.len(),
            &changed[..changed.len().min(15)]
        );
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
