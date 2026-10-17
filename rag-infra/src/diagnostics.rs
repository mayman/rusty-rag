//! The `Diagnostics` adapter: read-only facts about the data directory, the
//! database file, `git`, and the operation locks, for `rag doctor`. Nothing here
//! creates, changes, or deletes anything.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rag_core::application::error::StorageError;
use rag_core::application::ports::{DatabaseFacts, Diagnostics, GitFacts, Leftovers, LockState};
use rag_core::domain::source_id::SourceId;
use rusqlite::{Connection, OpenFlags};

use crate::config::DataLayout;
use crate::git::process::run;
use crate::lock::FileOperationLock;

/// How long `git --version` may take.
const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// The empty hooks directory the git adapter keeps in the scratch directory.
const HOOKS_DIR_NAME: &str = "no-hooks";
/// Most integrity problems quoted in one line.
const MAX_INTEGRITY_PROBLEMS: usize = 5;

/// `Diagnostics` over a data directory.
#[derive(Debug, Clone)]
pub struct SystemDiagnostics {
    layout: DataLayout,
}

impl SystemDiagnostics {
    pub fn new(layout: DataLayout) -> Self {
        Self { layout }
    }
}

impl Diagnostics for SystemDiagnostics {
    fn data_dir(&self) -> String {
        self.layout.root().display().to_string().replace('\\', "/")
    }

    fn check_data_dir(&self) -> Result<(), StorageError> {
        let root = self.layout.root();
        if !root.exists() {
            return Ok(());
        }
        std::fs::read_dir(root).map(drop).map_err(|error| {
            StorageError::Failed(
                format!(
                    "cannot read the data directory {}: {error}",
                    self.data_dir()
                )
                .into(),
            )
        })
    }

    fn database(&self, check_integrity: bool) -> DatabaseFacts {
        let file = self.layout.index_db();
        let Ok(metadata) = std::fs::metadata(&file) else {
            return DatabaseFacts::default();
        };
        let mut wal = file.clone().into_os_string();
        wal.push("-wal");
        DatabaseFacts {
            exists: true,
            size_bytes: metadata.len(),
            wal_bytes: std::fs::metadata(wal).map_or(0, |wal| wal.len()),
            integrity: check_integrity.then(|| integrity_check(&file)),
        }
    }

    fn git(&self) -> GitFacts {
        let mut command = Command::new("git");
        command
            .arg("--version")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        match run(command, None, GIT_PROBE_TIMEOUT) {
            Ok(output) if output.status.success() => GitFacts {
                on_path: true,
                version_text: String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .map(|line| line.trim().to_owned())
                    .filter(|line| !line.is_empty()),
            },
            Ok(_) | Err(_) => GitFacts::default(),
        }
    }

    fn checkout_present(&self, source: &SourceId) -> bool {
        self.layout
            .repos_dir()
            .join(source.checkout_dir_name())
            .is_dir()
    }

    fn operation_lock(&self, source: &SourceId) -> LockState {
        FileOperationLock::new(self.layout.locks_dir()).state_of(source.checkout_dir_name())
    }

    fn leftovers(&self, registered_checkouts: &[String]) -> Leftovers {
        let tmp_entries = entries(&self.layout.tmp_dir())
            .filter(|(name, _)| {
                name != HOOKS_DIR_NAME && !name.starts_with(crate::config::VOLUME_PROBE_PREFIX)
            })
            .count();
        let unregistered_checkouts = entries(&self.layout.repos_dir())
            .filter(|(name, is_dir)| *is_dir && !registered_checkouts.contains(name))
            .count();
        Leftovers {
            tmp_entries,
            unregistered_checkouts,
        }
    }
}

/// The names in `dir` and whether each is a directory; empty when it cannot be read.
fn entries(dir: &Path) -> impl Iterator<Item = (String, bool)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path().is_dir(),
            )
        })
}

/// `PRAGMA integrity_check` on a read-only connection, in one line: `ok`, or the
/// problems found (or why the check could not run).
fn integrity_check(file: &Path) -> String {
    let outcome = Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_ONLY).and_then(
        |connection| {
            let mut statement = connection.prepare("PRAGMA integrity_check")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<String>, _>>()
        },
    );
    match outcome {
        Ok(rows) if rows == ["ok"] => "ok".to_owned(),
        Ok(rows) => one_line(
            &rows
                .iter()
                .take(MAX_INTEGRITY_PROBLEMS)
                .cloned()
                .collect::<Vec<_>>()
                .join("; "),
        ),
        Err(error) => one_line(&error.to_string()),
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
