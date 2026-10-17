//! Per-source operation lock: an OS-level exclusive lock on a file under `locks/`.
//!
//! The operating system releases the lock when the file handle closes, which
//! happens on drop of the guard or when the process dies, so a crash never leaves a
//! stale lock behind. The lock file itself is never deleted (deleting a lock file
//! races with a waiter that already opened it).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;

use rag_core::application::error::StorageError;
use rag_core::application::ports::{LockGuard, OperationLock};
use rag_core::domain::source_id::SourceId;

/// `OperationLock` over lock files in one directory.
#[derive(Debug, Clone)]
pub struct FileOperationLock {
    dir: PathBuf,
}

impl FileOperationLock {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

/// Holds the open (and locked) file; closing it releases the lock.
struct HeldFile {
    _file: File,
}

impl OperationLock for FileOperationLock {
    fn acquire(&self, source: &SourceId) -> Result<LockGuard, StorageError> {
        std::fs::create_dir_all(&self.dir).map_err(failed)?;
        let path = self
            .dir
            .join(format!("{}.lock", source.checkout_dir_name()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(failed)?;
        // `File::try_lock` is stable since Rust 1.89; the pinned toolchain is 1.99.1
        // but the workspace declares `rust-version = "1.85"`. Remove this allow when
        // the workspace MSRV is raised to 1.89 (see the step 5a report).
        #[allow(clippy::incompatible_msrv)]
        let locked = file.try_lock();
        match locked {
            Ok(()) => Ok(LockGuard::new(HeldFile { _file: file })),
            Err(TryLockError::WouldBlock) => Err(StorageError::Busy),
            Err(TryLockError::Error(error)) => Err(failed(error)),
        }
    }
}

fn failed(error: std::io::Error) -> StorageError {
    StorageError::Failed(format!("operation lock: {error}").into())
}
