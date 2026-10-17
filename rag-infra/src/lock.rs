//! Per-source operation lock: an OS-level exclusive lock on a file under `locks/`.
//!
//! The operating system releases the lock when the file handle closes, which
//! happens on drop of the guard or when the process dies, so a crash never leaves a
//! stale lock behind. The lock file itself is never deleted (deleting a lock file
//! races with a waiter that already opened it).
//!
//! A holder that says which operation it runs (`acquire_waiting`) leaves its name in
//! a small `<hash>.op` file beside the lock file, so a waiter that gives up can say
//! what it was waiting for. The name lives in its own file because a locked file
//! cannot be read through another handle on Windows.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rag_core::application::error::StorageError;
use rag_core::application::ports::{LockGuard, OperationLock};
use rag_core::domain::source_id::SourceId;

/// How often a waiter tries again.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

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

impl FileOperationLock {
    /// Takes the lock whose file is named after `name`, a checkout hash
    /// (`SourceId::checkout_dir_name`). `acquire` uses it; the scratch-directory
    /// cleanup uses it for staging directories, which are named by the same hash.
    pub fn try_acquire_named(&self, name: &str) -> Result<LockGuard, StorageError> {
        self.try_acquire_for(name, None)
    }

    /// As `try_acquire_named`, and records `operation` for a waiter to name (a lock
    /// taken without one clears any earlier name).
    fn try_acquire_for(
        &self,
        name: &str,
        operation: Option<&str>,
    ) -> Result<LockGuard, StorageError> {
        std::fs::create_dir_all(&self.dir).map_err(failed)?;
        let path = self.dir.join(format!("{name}.lock"));
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
            Ok(()) => {
                let operation_file = self.dir.join(format!("{name}.op"));
                // Best effort: a missing name only makes the error less specific.
                match operation {
                    Some(operation) => {
                        let _ = std::fs::write(&operation_file, operation);
                    }
                    None => {
                        let _ = std::fs::remove_file(&operation_file);
                    }
                }
                Ok(LockGuard::new(HeldFile { _file: file }))
            }
            Err(TryLockError::WouldBlock) => Err(StorageError::Busy),
            Err(TryLockError::Error(error)) => Err(failed(error)),
        }
    }

    /// The operation the holder of the lock called `name` said it is running.
    fn holder_operation(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.join(format!("{name}.op"))).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }
}

impl OperationLock for FileOperationLock {
    fn acquire(&self, source: &SourceId) -> Result<LockGuard, StorageError> {
        self.try_acquire_named(source.checkout_dir_name())
    }

    fn acquire_waiting(
        &self,
        source: &SourceId,
        operation: &str,
        max_wait: Duration,
    ) -> Result<LockGuard, StorageError> {
        let name = source.checkout_dir_name();
        let deadline = Instant::now() + max_wait;
        loop {
            match self.try_acquire_for(name, Some(operation)) {
                Err(StorageError::Busy) => {}
                other => return other,
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(match self.holder_operation(name) {
                    Some(holder) => StorageError::locked_by(&holder),
                    None => StorageError::Busy,
                });
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }
}

fn failed(error: std::io::Error) -> StorageError {
    StorageError::Failed(format!("operation lock: {error}").into())
}
