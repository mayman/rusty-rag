//! Shared fixtures for the tests of the indexing use cases: a world of in-memory
//! fakes with one remote, and a spying git client.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::application::add_source::{AddRequest, AddResponse, AddSource};
use crate::application::error::StorageError;
use crate::application::error::{AppError, GitError};
use crate::application::ports::{
    ChunkStore, GitClient, IndexChanges, LockGuard, OperationLock, ReindexedChunk,
    SourceRepository, TreeListing,
};
use crate::application::progress::{Phase, Progress};
use crate::application::reindex::Reindex;
use crate::application::remove_source::RemoveSource;
use crate::application::testkit::{FakeGitClient, FakeOperationLock, FixedClock, InMemoryStore};
use crate::application::update_source::UpdateSource;
use crate::domain::chunk::Chunk;
use crate::domain::chunk_id::ChunkId;
use crate::domain::git_location::GitLocation;
use crate::domain::git_ref::GitRef;
use crate::domain::index_versions::IndexVersions;
use crate::domain::indexed_at::IndexedAt;
use crate::domain::manifest::Manifest;
use crate::domain::oid::{BlobOid, CommitId};
use crate::domain::repo_path::RepoPath;
use crate::domain::source::{Source, SourceStatus};
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

    pub fn add_source_with<'a>(&'a self, progress: &'a dyn Progress) -> AddSource<'a> {
        self.add_source().with_progress(progress)
    }

    pub fn add_source_locked<'a>(&'a self, locks: &'a dyn OperationLock) -> AddSource<'a> {
        AddSource::new(&self.store, &self.store, &self.git, locks, &self.clock)
    }

    pub fn update_source_locked<'a>(&'a self, locks: &'a dyn OperationLock) -> UpdateSource<'a> {
        UpdateSource::new(&self.store, &self.store, &self.git, locks, &self.clock)
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

    pub fn remove_source_locked<'a>(&'a self, locks: &'a dyn OperationLock) -> RemoveSource<'a> {
        RemoveSource::new(&self.store, &self.store, &self.git, locks)
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

/// A progress that writes every call down and reports 5 ms for each phase.
#[derive(Default)]
pub struct RecordingProgress {
    events: Mutex<Vec<String>>,
}

impl RecordingProgress {
    /// `kind:phase` (`begin:fetch`, `detail:list`, `end:read`) of every call, in order.
    pub fn sequence(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.splitn(3, ':').take(2).collect::<Vec<_>>().join(":"))
            .collect()
    }

    /// The full `kind:phase:text` of every call.
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl Progress for RecordingProgress {
    fn begin(&self, phase: Phase, detail: &str) {
        self.events
            .lock()
            .unwrap()
            .push(format!("begin:{}:{detail}", phase.as_str()));
    }

    fn detail(&self, phase: Phase, text: &str) {
        self.events
            .lock()
            .unwrap()
            .push(format!("detail:{}:{text}", phase.as_str()));
    }

    fn end(&self, phase: Phase) -> u64 {
        self.events
            .lock()
            .unwrap()
            .push(format!("end:{}:", phase.as_str()));
        5
    }
}

/// A lock that forwards to another and records every `acquire_waiting` it is asked:
/// the operation and the longest wait.
pub struct RecordingLock<'a> {
    inner: &'a dyn OperationLock,
    waits: Mutex<Vec<(String, Duration)>>,
}

impl<'a> RecordingLock<'a> {
    pub fn new(inner: &'a dyn OperationLock) -> Self {
        Self {
            inner,
            waits: Mutex::new(Vec::new()),
        }
    }

    pub fn waits(&self) -> Vec<(String, Duration)> {
        self.waits.lock().unwrap().clone()
    }
}

impl OperationLock for RecordingLock<'_> {
    fn acquire(&self, source: &SourceId) -> Result<LockGuard, StorageError> {
        self.inner.acquire(source)
    }

    fn acquire_waiting(
        &self,
        source: &SourceId,
        operation: &str,
        max_wait: Duration,
    ) -> Result<LockGuard, StorageError> {
        self.waits
            .lock()
            .unwrap()
            .push((operation.to_owned(), max_wait));
        self.inner.acquire_waiting(source, operation, max_wait)
    }
}

/// A store that forwards to an `InMemoryStore` and can misbehave on request: fail
/// every `commit_index` with a given error, pretend one source was removed after it
/// was listed, and write down the status of every `save`.
pub struct SpiedStore<'a> {
    inner: &'a InMemoryStore,
    commit_error: Option<StorageError>,
    hidden: Option<SourceId>,
    saved: Mutex<Vec<SourceStatus>>,
}

impl<'a> SpiedStore<'a> {
    pub fn new(inner: &'a InMemoryStore) -> Self {
        Self {
            inner,
            commit_error: None,
            hidden: None,
            saved: Mutex::new(Vec::new()),
        }
    }

    /// Every `commit_index` fails with `error`, changing nothing.
    pub fn failing_commits(mut self, error: StorageError) -> Self {
        self.commit_error = Some(error);
        self
    }

    /// `get` says `id` does not exist, though `list` still names it.
    pub fn hiding(mut self, id: &SourceId) -> Self {
        self.hidden = Some(id.clone());
        self
    }

    /// The status of every source passed to `save`, in order.
    pub fn saved_statuses(&self) -> Vec<SourceStatus> {
        self.saved.lock().unwrap().clone()
    }
}

impl SourceRepository for SpiedStore<'_> {
    fn get(&self, id: &SourceId) -> Result<Option<Source>, StorageError> {
        if self.hidden.as_ref() == Some(id) {
            return Ok(None);
        }
        self.inner.get(id)
    }

    fn find_by_name(&self, name: &str) -> Result<Option<Source>, StorageError> {
        self.inner.find_by_name(name)
    }

    fn list(&self) -> Result<Vec<Source>, StorageError> {
        self.inner.list()
    }

    fn save(&self, source: &Source) -> Result<(), StorageError> {
        self.saved.lock().unwrap().push(source.status());
        self.inner.save(source)
    }
}

impl ChunkStore for SpiedStore<'_> {
    fn commit_index(
        &self,
        source: &Source,
        expected_base: Option<&CommitId>,
        changes: &IndexChanges,
    ) -> Result<(), StorageError> {
        match &self.commit_error {
            Some(error) => Err(error.clone()),
            None => self.inner.commit_index(source, expected_base, changes),
        }
    }

    fn remove_source(&self, id: &SourceId) -> Result<(), StorageError> {
        self.inner.remove_source(id)
    }

    fn manifest(&self, source: &SourceId) -> Result<Manifest, StorageError> {
        self.inner.manifest(source)
    }

    fn chunk_count(&self, source: &SourceId) -> Result<usize, StorageError> {
        self.inner.chunk_count(source)
    }

    fn chunk_count_for_paths(
        &self,
        source: &SourceId,
        paths: &[RepoPath],
    ) -> Result<usize, StorageError> {
        self.inner.chunk_count_for_paths(source, paths)
    }

    fn chunk(&self, id: &ChunkId) -> Result<Option<Chunk>, StorageError> {
        self.inner.chunk(id)
    }

    fn chunks(&self, source: &SourceId) -> Result<Vec<Chunk>, StorageError> {
        self.inner.chunks(source)
    }

    fn stored_versions(&self, source: &SourceId) -> Result<Option<IndexVersions>, StorageError> {
        self.inner.stored_versions(source)
    }

    fn rewrite_index_fields(
        &self,
        source: &SourceId,
        fields: &[ReindexedChunk],
        versions: IndexVersions,
    ) -> Result<(), StorageError> {
        self.inner.rewrite_index_fields(source, fields, versions)
    }
}
