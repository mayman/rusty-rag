//! An in-memory `Diagnostics`: the host facts of `rag doctor` set by the test.

use std::collections::{BTreeMap, BTreeSet};

use crate::application::error::StorageError;
use crate::application::ports::{DatabaseFacts, Diagnostics, GitFacts, Leftovers, LockState};
use crate::domain::source_id::SourceId;

/// A healthy host unless told otherwise: the database exists, git 2.55 is on the
/// path, no clone is present (use `with_checkout`), no lock is held, nothing is left
/// over.
#[derive(Debug, Clone)]
pub struct FakeDiagnostics {
    data_dir: String,
    unreadable: Option<String>,
    database: DatabaseFacts,
    integrity_report: String,
    git: GitFacts,
    repos_dirs: BTreeSet<String>,
    held: BTreeMap<String, Option<String>>,
    tmp_entries: usize,
}

impl Default for FakeDiagnostics {
    fn default() -> Self {
        Self {
            data_dir: "/data/rag".to_owned(),
            unreadable: None,
            database: DatabaseFacts {
                exists: true,
                size_bytes: 8192,
                wal_bytes: 0,
                integrity: None,
            },
            integrity_report: "ok".to_owned(),
            git: GitFacts {
                on_path: true,
                version_text: Some("git version 2.55.0".to_owned()),
            },
            repos_dirs: BTreeSet::new(),
            held: BTreeMap::new(),
            tmp_entries: 0,
        }
    }
}

impl FakeDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// The data directory exists but cannot be read.
    pub fn unreadable(mut self, why: &str) -> Self {
        self.unreadable = Some(why.to_owned());
        self
    }

    /// No database file yet.
    pub fn without_database(mut self) -> Self {
        self.database = DatabaseFacts::default();
        self
    }

    pub fn with_database_sizes(mut self, size_bytes: u64, wal_bytes: u64) -> Self {
        self.database.size_bytes = size_bytes;
        self.database.wal_bytes = wal_bytes;
        self
    }

    /// What the integrity check says when it is asked for.
    pub fn with_integrity_report(mut self, report: &str) -> Self {
        self.integrity_report = report.to_owned();
        self
    }

    /// `git` cannot be started.
    pub fn without_git(mut self) -> Self {
        self.git = GitFacts::default();
        self
    }

    /// `git --version` printed `text`.
    pub fn with_git_version_text(mut self, text: &str) -> Self {
        self.git = GitFacts {
            on_path: true,
            version_text: Some(text.to_owned()),
        };
        self
    }

    /// The clone of `source` is in place.
    pub fn with_checkout(mut self, source: &SourceId) -> Self {
        self.repos_dirs
            .insert(source.checkout_dir_name().to_owned());
        self
    }

    /// A clone directory in `repos/` under any name.
    pub fn with_repos_dir(mut self, name: &str) -> Self {
        self.repos_dirs.insert(name.to_owned());
        self
    }

    /// The operation lock of `source` is held by `operation`.
    pub fn with_lock_held(mut self, source: &SourceId, operation: Option<&str>) -> Self {
        self.held.insert(
            source.checkout_dir_name().to_owned(),
            operation.map(str::to_owned),
        );
        self
    }

    pub fn with_tmp_entries(mut self, count: usize) -> Self {
        self.tmp_entries = count;
        self
    }
}

impl Diagnostics for FakeDiagnostics {
    fn data_dir(&self) -> String {
        self.data_dir.clone()
    }

    fn check_data_dir(&self) -> Result<(), StorageError> {
        match &self.unreadable {
            Some(why) => Err(StorageError::Failed(why.as_str().into())),
            None => Ok(()),
        }
    }

    fn database(&self, check_integrity: bool) -> DatabaseFacts {
        let mut facts = self.database.clone();
        if check_integrity && facts.exists {
            facts.integrity = Some(self.integrity_report.clone());
        }
        facts
    }

    fn git(&self) -> GitFacts {
        self.git.clone()
    }

    fn checkout_present(&self, source: &SourceId) -> bool {
        self.repos_dirs.contains(source.checkout_dir_name())
    }

    fn operation_lock(&self, source: &SourceId) -> LockState {
        match self.held.get(source.checkout_dir_name()) {
            Some(operation) => LockState::Held(operation.clone()),
            None => LockState::Free,
        }
    }

    fn leftovers(&self, registered_checkouts: &[String]) -> Leftovers {
        Leftovers {
            tmp_entries: self.tmp_entries,
            unregistered_checkouts: self
                .repos_dirs
                .iter()
                .filter(|name| !registered_checkouts.contains(name))
                .count(),
        }
    }
}
