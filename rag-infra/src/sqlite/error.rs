//! Translating `rusqlite` errors into the application's `StorageError` at the
//! adapter boundary. No `rusqlite::Error` leaves this module's callers.

use rag_core::application::error::StorageError;
use rusqlite::ErrorCode;

/// `Busy` when another connection holds the write lock (or the file is locked),
/// `Failed` for everything else. The message is scrubbed by `StorageError`.
pub(crate) fn storage_error(error: rusqlite::Error) -> StorageError {
    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StorageError::Busy,
        _ => failed(format!("database error: {error}")),
    }
}

pub(crate) fn failed(message: impl Into<String>) -> StorageError {
    StorageError::Failed(message.into().into())
}
