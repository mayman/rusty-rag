//! Shared fixtures for the tests of the indexing use cases: a world of in-memory
//! fakes with one remote, and a spying git client.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::application::add_source::{AddRequest, AddResponse, AddSource};
use crate::application::error::{AppError, GitError};
use crate::application::ports::{GitClient, TreeListing};
use crate::application::reindex::Reindex;
use crate::application::remove_source::RemoveSource;
use crate::application::testkit::{FakeGitClient, FakeOperationLock, FixedClock, InMemoryStore};
use crate::application::update_source::UpdateSource;
use crate::domain::git_location::GitLocation;
use crate::domain::git_ref::GitRef;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::oid::{BlobOid, CommitId};
use crate::domain::source_id::SourceId;

pub const REMOTE: &str = "https://example.com/o/r";

pub fn location(remote: &str) -> GitLocation {
    GitLocation::parse(remote).unwrap()
}

pub fn id_of(remote: &str) -> SourceId {
    SourceId::from_location(&location(remote)).unwrap()
}

/// Every in-memory fake, with one remote (`REMOTE`, branch `main`).
pub struct World {
    pub store: InMemoryStore,
    pub git: FakeGitClient,
    pub locks: FakeOperationLock,
    pub clock: FixedClock,
}

impl World {
    pub fn new() -> Self {
        let git = FakeGitClient::new();
        git.add_remote(REMOTE, "main");
        Self {
            store: InMemoryStore::new(),
            git,
            locks: FakeOperationLock::new(),
            clock: FixedClock::new(IndexedAt::from_epoch_seconds(1_000)),
        }
    }

    pub fn id(&self) -> SourceId {
        id_of(REMOTE)
    }

    pub fn commit(&self, files: &[(&str, &str)]) -> CommitId {
        self.git.commit(REMOTE, files)
    }

    pub fn add_source(&self) -> AddSource<'_> {
        self.add_source_via(&self.git)
    }

    pub fn add_source_via<'a>(&'a self, git: &'a dyn GitClient) -> AddSource<'a> {
        AddSource::new(&self.store, &self.store, git, &self.locks, &self.clock)
    }

    pub fn update_source(&self) -> UpdateSource<'_> {
        self.update_source_via(&self.git)
    }

    pub fn update_source_via<'a>(&'a self, git: &'a dyn GitClient) -> UpdateSource<'a> {
        UpdateSource::new(&self.store, &self.store, git, &self.locks, &self.clock)
    }

    pub fn remove_source(&self) -> RemoveSource<'_> {
        self.remove_source_via(&self.git)
    }

    pub fn remove_source_via<'a>(&'a self, git: &'a dyn GitClient) -> RemoveSource<'a> {
        RemoveSource::new(&self.store, &self.store, git, &self.locks)
    }

    pub fn reindex(&self) -> Reindex<'_> {
        self.reindex_via(&self.git)
    }

    pub fn reindex_via<'a>(&'a self, git: &'a dyn GitClient) -> Reindex<'a> {
        Reindex::new(&self.store, &self.store, git, &self.locks, &self.clock)
    }

    /// Adds `REMOTE` under its default name.
    pub fn add(&self) -> Result<AddResponse, AppError> {
        self.add_source().execute(&AddRequest::new(REMOTE))
    }
}

/// A git client that forwards to another and records what the use cases ask of it.
pub struct GitSpy<'a> {
    inner: &'a dyn GitClient,
    calls: Mutex<Vec<&'static str>>,
    reads: Mutex<Vec<(usize, u64)>>,
    hidden_commit: Mutex<Option<CommitId>>,
}

impl<'a> GitSpy<'a> {
    pub fn new(inner: &'a dyn GitClient) -> Self {
        Self {
            inner,
            calls: Mutex::new(Vec::new()),
            reads: Mutex::new(Vec::new()),
            hidden_commit: Mutex::new(None),
        }
    }

    /// Makes `ls_tree` and `read_blobs` fail with `NotFound` for the objects of
    /// `commit`, as when git has dropped them.
    pub fn hide_commit(&self, commit: &CommitId) {
        *self.hidden_commit.lock().unwrap() = Some(commit.clone());
    }

    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    pub fn called(&self, operation: &str) -> bool {
        self.calls().contains(&operation)
    }

    /// For each `read_blobs` call: how many blobs it asked for and their total
    /// size in bytes.
    pub fn reads(&self) -> Vec<(usize, u64)> {
        self.reads.lock().unwrap().clone()
    }

    fn note(&self, operation: &'static str) {
        self.calls.lock().unwrap().push(operation);
    }
}

impl GitClient for GitSpy<'_> {
    fn default_branch(&self, location: &GitLocation) -> Result<GitRef, GitError> {
        self.note("default_branch");
        self.inner.default_branch(location)
    }

    fn clone_shallow(
        &self,
        source: &SourceId,
        location: &GitLocation,
        git_ref: &GitRef,
    ) -> Result<CommitId, GitError> {
        self.note("clone_shallow");
        self.inner.clone_shallow(source, location, git_ref)
    }

    fn fetch_update(&self, source: &SourceId, git_ref: &GitRef) -> Result<CommitId, GitError> {
        self.note("fetch_update");
        self.inner.fetch_update(source, git_ref)
    }

    fn ls_tree(&self, source: &SourceId, commit: &CommitId) -> Result<TreeListing, GitError> {
        self.note("ls_tree");
        if self.hidden_commit.lock().unwrap().as_ref() == Some(commit) {
            return Err(GitError::NotFound(format!("commit {commit}").into()));
        }
        self.inner.ls_tree(source, commit)
    }

    fn read_blobs(
        &self,
        source: &SourceId,
        blobs: &[BlobOid],
    ) -> Result<BTreeMap<BlobOid, Vec<u8>>, GitError> {
        self.note("read_blobs");
        let contents = self.inner.read_blobs(source, blobs)?;
        let bytes = contents.values().map(|c| c.len() as u64).sum();
        self.reads.lock().unwrap().push((blobs.len(), bytes));
        Ok(contents)
    }

    fn remove_checkout(&self, source: &SourceId) -> Result<(), GitError> {
        self.note("remove_checkout");
        self.inner.remove_checkout(source)
    }
}
