//! Infrastructure layer for `rag`: adapters that implement the ports defined
//! in `rag-core::application` (SQLite/FTS5 store, git subprocess client,
//! filesystem walker, lock file, system clock, configuration).

pub mod clock;
pub mod config;
pub mod git;
pub mod lock;
pub mod sqlite;
#[doc(hidden)]
pub mod testing;
