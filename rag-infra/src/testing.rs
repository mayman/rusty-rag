//! Support for end-to-end tests of other crates. Not part of the adapter surface.
//!
//! A real index can only be made stale by an older build; no port stamps other
//! tokenizer, chunker, or policy versions. `age_index` does it with a second
//! connection, as an older build's database would look.

use std::path::Path;
use std::time::Duration;

use rag_core::domain::index_versions::IndexVersions;

use crate::sqlite::DATABASE_FILE;

/// Stamps every indexed source of the database in `directory` with `versions`.
///
/// # Panics
/// When the database cannot be opened or updated: this is test support.
pub fn age_index(directory: &Path, versions: IndexVersions) {
    let connection =
        rusqlite::Connection::open(directory.join(DATABASE_FILE)).expect("open the database");
    connection
        .busy_timeout(Duration::from_secs(10))
        .expect("busy timeout");
    connection
        .execute(
            "UPDATE sources SET tokenizer_version = ?1, chunker_version = ?2, \
             policy_version = ?3 WHERE indexed_commit IS NOT NULL",
            (versions.tokenizer, versions.chunker, versions.policy),
        )
        .expect("stamp the versions");
}
